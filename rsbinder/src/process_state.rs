// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs::File;
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{self, Arc, OnceLock, RwLock};
use std::thread;

use crate::{binder::*, error::*, proxy::*, sys::binder, thread_state};

/// Best-effort undo of the case (a) `BC_INCREFS` pin after a failure
/// downstream of the pin's flush (descriptor query failure or
/// `ProxyHandle::new_acquired` failure). If the kernel ack of the
/// undo command is itself lost — driver write_read ioctl failure —
/// we log and accept that the pin leaks until obituary or process
/// teardown. The alternative (returning the secondary error) would
/// mask the original failure that triggered the undo.
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

/// Plan computed under P1's write lock and consumed by P2/P3.
///
/// Drives the lock-decoupled three-phase slow path: the case decision
/// is made under one short write-lock window, IPC runs with the lock
/// released, and a second short write-lock window commits the cache
/// entry while re-checking cross-thread races.
enum SlowPathPlan {
    /// Sub-case (a): entry absent at P1 time. P1 issued `BC_INCREFS`
    /// then flushed, so this thread now owns the cache pin. P3 either
    /// transfers ownership to the cache entry on insert, or undoes the
    /// pin via [`undo_case_a_pin`] on failure / cross-thread race.
    CaseA,
    /// Sub-case (b): entry present at P1 time but its `weak` is
    /// dangling. The cache pin remains owned by the existing entry —
    /// this thread does not issue or own a pin. P1 snapshotted the
    /// entry's descriptor and generation; P2 skips
    /// `query_interface` (descriptor immutable for the binder_ref
    /// slot's lifetime) and P3 resurrects under the same generation
    /// when the snapshot still matches.
    CaseB { descriptor: String, generation: u64 },
}

/// Outcome of P1: either we observed a live entry and the slow path
/// is done, or we have a [`SlowPathPlan`] to drive P2/P3.
enum SlowPathDecision {
    /// Sub-case (c): another thread inserted/upgraded between the
    /// read-fast-path miss and the P1 write-lock acquisition.
    Cached(SIBinder),
    /// Sub-cases (a)/(b) — proceed to P2 (IPC) and P3 (commit).
    NeedIpc(SlowPathPlan),
}

/// Output of P2 — carries the descriptor each branch needs into P3,
/// so the (CaseA, _) commit arms can consume the freshly-queried
/// descriptor without going through an `Option<String>` that P3
/// would otherwise have to runtime-`expect` for `(CaseA, None)`.
enum SlowPathReady {
    /// Sub-case (a) ready for commit: P2 issued `query_interface`
    /// and obtained a fresh descriptor.
    CaseA { descriptor: String },
    /// Sub-case (b) ready for commit: descriptor and generation
    /// snapshotted by P1, no IPC required in P2.
    CaseB { descriptor: String, generation: u64 },
}

/// RAII guard that restores this thread's [`CallRestriction`] to its
/// pre-call value when dropped. Applied around `ping_binder(0)` in
/// P2 so an early-`Err` return or panic cannot leak
/// [`CallRestriction::None`] into subsequent calls on this thread.
struct RestoreCallRestriction(CallRestriction);

impl Drop for RestoreCallRestriction {
    fn drop(&mut self) {
        thread_state::set_call_restriction(self.0);
    }
}

// Test-only hook fired at the entry of `ProcessState::slow_path_p2`
// (immediately after P1 releases the `handle_to_proxy` write lock
// and before P2 issues any IPC). Used by the same-thread deadlock
// regression test to invoke `send_obituary_for_handle` on the same
// thread that is currently in the slow path — the pre-fix code held
// the write lock across this point and would deadlock on the
// obituary's `handle_to_proxy.write()` re-entry; the post-fix split
// releases the lock during P2 so the obituary acquires it cleanly.
// Empty-by-default; tests install a closure via
// `set_slow_path_p2_test_hook`.
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
    // Take the closure out before invoking so the hook body can
    // re-enter `strong_proxy_for_handle` (and thus this function)
    // without a nested-borrow panic on the RefCell.
    let hook = SLOW_PATH_P2_TEST_HOOK.with(|h| h.borrow_mut().take());
    if let Some(mut hook) = hook {
        hook(handle);
        SLOW_PATH_P2_TEST_HOOK.with(|h| *h.borrow_mut() = Some(hook));
    }
}

/// P3 commit primitive: acquire one kernel strong ref
/// (`BC_ACQUIRE`) and insert (or replace) the cache entry. Caller
/// holds the `handle_to_proxy` write lock.
///
/// `owns_case_a_pin` records whether *this thread* issued the
/// case (a) `BC_INCREFS` pin during P1; on `new_acquired` failure it
/// gates whether to undo our own pin. Case (b) and the cross-thread
/// `(CaseA, Some(_))` race both pass `false` because the existing
/// cache entry's pin is owned by the entry, not by us.
fn commit_new_acquired(
    handle_to_proxy: &mut HashMap<u32, CacheEntry>,
    handle: u32,
    descriptor: String,
    generation: u64,
    stability: Stability,
    owns_case_a_pin: bool,
) -> Result<SIBinder> {
    let arc = match ProxyHandle::new_acquired(handle, descriptor.clone(), stability) {
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

/// Per-handle cache entry under the cache-pin model.
///
/// `weak` lets the process resurrect a fresh `Arc<ProxyHandle>` after the
/// previous one has been dropped, without issuing a new
/// `INTERFACE_TRANSACTION` (the cached `descriptor` is reused).
///
/// The kernel weak ref (`BC_INCREFS`) that keeps `binder_ref(handle)`
/// alive while user-side strong count is 0 is **not** a separate field —
/// it is owned implicitly by this entry's presence in
/// `handle_to_proxy`. The pin is acquired exactly once on first
/// insertion (slow-path case (a)) and released exactly once on obituary
/// teardown.
///
/// **Slow-path lock discipline.** The slow path is split into three
/// phases (P1/P2/P3) so that the descriptor query (and the
/// `ping_binder(0)` issued for service manager on Android sdk>=30)
/// runs with `handle_to_proxy` *unlocked*. This is required for
/// re-entrancy: the catch-all arm of `wait_for_response` dispatches
/// `BR_DEAD_BINDER` to `execute_command`, which calls
/// [`ProcessState::send_obituary_for_handle`] and re-acquires the
/// same write lock. Holding the lock across IPC would deadlock under
/// `std::sync::RwLock`'s non-reentrant write semantics.
///
/// To preserve the cache pin invariant under that split, P3 covers a
/// race where another thread (T2) ran a complete case (a) during our
/// P2 IPC and then dropped its `Arc`: when our P3 plan was CaseA but
/// the cache slot is again "present + dangling weak", we have one
/// spare BC_INCREFS pin (ours) on top of T2's cache-owned pin. P3
/// undoes our pin and adopts T2's descriptor/generation, restoring
/// "entry-1 ↔ pin-1". The companion `(CaseB, None)` arm — cache
/// entry vanished mid-flight via obituary — returns `DeadObject`
/// rather than racing BC_ACQUIRE against a freed binder_ref slot;
/// this is the one window where the "BC_ACQUIRE precondition = pin
/// alive" invariant could otherwise break.
///
/// `generation` is a process-wide monotonic counter snapshotted at
/// case-(a) insertion. It enables `WIBinder::upgrade()` to detect when
/// the same handle id has been recycled to a different `binder_node` —
/// resurrection through case (b) is only safe when the snapshot taken
/// at `SIBinder::downgrade` time still matches the live entry's
/// generation. Case (b) preserves the existing entry's generation
/// (same kernel slot, just user-space resurrection); only case (a)
/// allocates a new generation.
pub(crate) struct CacheEntry {
    pub(crate) weak: sync::Weak<ProxyHandle>,
    pub(crate) descriptor: String,
    pub(crate) generation: u64,
}

/// Sidecar-table entry for a native binder this process has published.
///
/// Replaces the previous fat-pointer encoding (`flat_binder_object.binder` =
/// data pointer, `flat_binder_object.cookie` = vtable pointer) with a
/// process-monotonic u64 id. The id is what the kernel echoes back in
/// `BR_INCREFS` / `BR_ACQUIRE` / `BR_RELEASE` / `BR_DECREFS` /
/// `BR_TRANSACTION` (`target.ptr`); lookup resolves to the live Arc via
/// `binder_pin.as_arc()`. Closes a UAF where weak-ref BR handlers
/// (`BR_DECREFS`) could fire after the underlying `Inner<T>` had been
/// dropped under the old encoding — see Android's two-allocation
/// (`weakref_type*` + `BBinder*`) design for the canonical fix shape.
///
/// Lifecycle: created on first `From<&SIBinder>` (BINDER_TYPE_BINDER), held
/// as long as either parcel-side (`publish_count`) or kernel-side
/// (`kernel_refs`) refs are outstanding, removed when both reach zero.
/// While the entry exists, `binder_pin` keeps `Inner<T>` alive and
/// `RefCounter.strong` / `RefCounter.weak` sit at the binary "alive"
/// level (>= 1) so that `attempt_inc_*` succeeds.
pub(crate) struct PublishedNative {
    /// Owns `RefCounter.strong` >= 1 via `SIBinder::from_arc`'s
    /// `inc_strong` (entry creation) and `SIBinder::Drop`'s
    /// `dec_strong(None)` (entry removal). Also keeps the underlying
    /// `Arc<dyn IBinder>` strong > 0 — this is the canonical reference
    /// that keeps `Inner<T>` alive while the kernel or any outgoing
    /// parcel still references the published binder.
    pub(crate) binder_pin: SIBinder,
    /// Number of live `flat_binder_object` instances of type
    /// `BINDER_TYPE_BINDER` for this id across all parcel buffers in
    /// this process. Driven by `flat_binder_object::acquire` /
    /// `release` (the existing pair already invoked from
    /// `Parcel::write_object`, `Parcel::append_from`, and
    /// `Parcel::release_objects`).
    pub(crate) publish_count: u32,
    /// Number of outstanding kernel refs against this id.
    /// `BR_INCREFS` / `BR_ACQUIRE` increment; `BR_RELEASE` /
    /// `BR_DECREFS` decrement (deferred via `pending_*_derefs`,
    /// processed FIFO).
    pub(crate) kernel_refs: u32,
}

#[derive(Debug, Clone, Copy)]
pub enum CallRestriction {
    // all calls okay
    None,
    // log when calls are blocking
    ErrorIfNotOneway,
    // abort process on blocking calls
    FatalIfNotOneway,
}

const DEFAULT_MAX_BINDER_THREADS: u32 = 15;
const DEFAULT_ENABLE_ONEWAY_SPAM_DETECTION: u32 = 1;

struct MemoryMap {
    ptr: *mut c_void,
    size: usize,
}
// SAFETY: `ptr` is a PROT_READ binder mapping owned for the whole
// ProcessState lifetime. It is never written through and never read as
// Rust data (the binder driver manages the pages); the only use is the
// single `munmap(ptr, size)` in ProcessState::drop. No data race is
// possible, so the handle is safe to send and share across threads.
unsafe impl Sync for MemoryMap {}
unsafe impl Send for MemoryMap {}

pub struct ProcessState {
    max_threads: u32,
    driver_name: PathBuf,
    driver: Arc<File>,
    mmap: RwLock<MemoryMap>,
    context_manager: RwLock<Option<SIBinder>>,
    handle_to_proxy: RwLock<HashMap<u32, CacheEntry>>,
    /// Monotonic counter for `CacheEntry::generation`. Incremented
    /// exactly once per case-(a) cache insertion (i.e. per fresh
    /// `BC_INCREFS` pin). Wrap-around is not a practical concern (u64).
    next_generation: AtomicU64,
    /// Native binders this process has published, keyed by a
    /// process-monotonic u64 id encoded in `flat_binder_object.binder`
    /// (replacing the previous fat-pointer encoding). Lookup resolves
    /// the id to a live `Arc<dyn IBinder>` for `BR_TRANSACTION` /
    /// `BR_INCREFS` / `BR_ACQUIRE` / `BR_RELEASE` / `BR_DECREFS` /
    /// `BR_ATTEMPT_ACQUIRE` and for round-trip
    /// `BINDER_TYPE_BINDER` deserialization. See
    /// `PublishedNative` for entry-lifecycle invariants.
    published_natives: RwLock<HashMap<u64, PublishedNative>>,
    /// Monotonic id allocator for `published_natives`. u64 wrap-around
    /// is not a practical concern.
    next_native_id: AtomicU64,
    disable_background_scheduling: AtomicBool,
    call_restriction: RwLock<CallRestriction>,
    thread_pool_started: AtomicBool,
    thread_pool_seq: AtomicUsize,
    kernel_started_threads: AtomicUsize,
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
    /// initialized (`init`/`init_default` called).
    ///
    /// Additive, read-only, and **not used by the kernel path** — its
    /// only caller is the `Tokio` async pool's "are we currently
    /// servicing a kernel binder transaction?" guard, which must answer
    /// `false` (instead of panicking via [`as_self`]) in a pure RPC
    /// process that never brought up kernel binder. When `ProcessState`
    /// *is* initialized this returns `true`, so every kernel scenario is
    /// byte-for-byte the prior behavior.
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

    fn inner_init(
        driver_name: &str,
        max_threads: u32,
    ) -> std::result::Result<ProcessState, Box<dyn std::error::Error>> {
        let max_threads = if max_threads != 0 && max_threads < DEFAULT_MAX_BINDER_THREADS {
            max_threads
        } else {
            DEFAULT_MAX_BINDER_THREADS
        };

        let driver_name = PathBuf::from(driver_name);

        let driver = open_driver(&driver_name, max_threads)?;

        let vm_size = (1024 * 1024) - rustix::param::page_size() * 2;
        // let vm_size = std::num::NonZeroUsize::new(vm_size).ok_or("vm_size is zero!")?;

        // SAFETY: `mmap` is unsafe because it creates a new mapping. `driver`
        // is a live, open binder device fd; `vm_size > 0`; addr is null so
        // the kernel chooses the address; PROT_READ + MAP_PRIVATE means the
        // region is never written through this pointer; offset 0 is the
        // binder ABI contract. The result is `?`-checked, and the mapping is
        // unmapped exactly once in ProcessState::drop.
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
            current_threads: AtomicUsize::new(0),
        })
    }

    /// Initialize ProcessState with binder path and max threads.
    /// The meaning of zero max threads is to use the default value. It is dependent on the kernel.
    /// If you want to use the default binder path, use init_default().
    pub fn init(
        driver_name: &str,
        max_threads: u32,
    ) -> std::result::Result<&'static ProcessState, Box<dyn std::error::Error>> {
        let cell = Self::instance();
        if let Some(existing) = cell.get() {
            return Ok(existing);
        }
        // Build outside the cell so a failed init is NOT cached: a later
        // call can retry (this is why `get_or_try_init`, still unstable,
        // is avoided). If two threads race here, `get_or_init` keeps the
        // first stored instance and the extra one is dropped.
        let instance = Self::inner_init(driver_name, max_threads)?;
        Ok(cell.get_or_init(|| instance))
    }

    /// Initialize ProcessState with default binder path and max threads.
    /// The meaning of zero max threads is to use the default value. It is dependent on the kernel.
    /// DEFAULT_BINDER_PATH is "/dev/binderfs/binder".
    pub fn init_default() -> std::result::Result<&'static ProcessState, Box<dyn std::error::Error>>
    {
        let path = if Path::new(crate::DEFAULT_BINDER_PATH).exists() {
            crate::DEFAULT_BINDER_PATH
        } else {
            crate::LEGACY_BINDER_PATH
        };
        Self::init(path, 0)
    }

    /// Get binder service manager.
    pub fn become_context_manager(
        &self,
        binder: SIBinder,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut context_manager = self
            .context_manager
            .write()
            .expect("Context manager lock poisoned");

        if context_manager.is_none() {
            let obj = binder::flat_binder_object::new_binder_with_flags(
                binder::FLAT_BINDER_FLAG_ACCEPTS_FDS,
            );

            if binder::set_context_mgr_ext(&self.driver, obj).is_err() {
                //     android_errorWriteLog(0x534e4554, "121035042");
                // let unused: i32 = 0;
                if let Err(e) = binder::set_context_mgr(&self.driver, 0) {
                    return Err(
                        format!("Binder ioctl to become context manager failed: {e}").into(),
                    );
                }
            }
            *context_manager = Some(binder);
        }

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
        // Read-lock fast path: pure Arc::clone, no kernel command. Common
        // case under steady-state load.
        if let Some(arc) = self
            .handle_to_proxy
            .read()
            .expect("Handle to proxy lock poisoned")
            .get(&handle)
            .and_then(|e| e.weak.upgrade())
        {
            return Ok(SIBinder::from_arc(arc));
        }

        // Slow path: lock-decoupled three phases.
        //
        //   P1: short write-lock window. Decide the sub-case and, for
        //        case (a), issue BC_INCREFS + flush so the cache pin
        //        is live in the kernel before any IPC enters.
        //        flush_commands is a write-only ioctl (read_size = 0),
        //        so no BR_* — including BR_DEAD_BINDER — can be
        //        dispatched while the lock is held; reentrant
        //        send_obituary_for_handle paths only originate from
        //        BR_DEAD_BINDER.
        //   P2: lock released. IPC (ping_binder for handle 0 sdk>=30
        //        and query_interface for case (a)) runs without the
        //        handle_to_proxy lock held, so a re-entrant
        //        BR_DEAD_BINDER → send_obituary_for_handle path can
        //        take the lock without deadlocking against us.
        //   P3: re-acquire write lock. Re-check case (c) and the
        //        case (a)→(b) cross-thread race, undo any spare pin,
        //        and commit the cache entry.
        let plan = match self.slow_path_p1(handle)? {
            SlowPathDecision::Cached(arc) => return Ok(arc),
            SlowPathDecision::NeedIpc(plan) => plan,
        };
        let ready = self.slow_path_p2(handle, plan)?;
        self.slow_path_p3(handle, stability, ready)
    }

    /// Slow-path phase 1: short write-lock window that decides the
    /// sub-case and, for case (a), issues the kernel cache pin
    /// (`BC_INCREFS` + `flush_commands`) before releasing the lock.
    fn slow_path_p1(&self, handle: u32) -> Result<SlowPathDecision> {
        // Write lock — even though P1 only reads, holding the write
        // lock here prevents two concurrent slow paths from both
        // observing "absent" and producing two case-(a) commits with
        // distinct generations. The write lock serializes the
        // BC_INCREFS issue point.
        let handle_to_proxy = self
            .handle_to_proxy
            .write()
            .expect("Handle to proxy lock poisoned");

        // Sub-case (c): another thread inserted/upgraded between the
        // read-fast-path miss and this write-lock acquisition.
        if let Some(arc) = handle_to_proxy.get(&handle).and_then(|e| e.weak.upgrade()) {
            return Ok(SlowPathDecision::Cached(SIBinder::from_arc(arc)));
        }

        // Sub-case (b): entry present with dangling weak. Snapshot the
        // descriptor/generation; the cache pin (BC_INCREFS issued at
        // first insertion) is still active so P3's BC_ACQUIRE will
        // succeed without needing a new pin.
        if let Some(entry) = handle_to_proxy.get(&handle) {
            return Ok(SlowPathDecision::NeedIpc(SlowPathPlan::CaseB {
                descriptor: entry.descriptor.clone(),
                generation: entry.generation,
            }));
        }

        // Sub-case (a): entry absent. Pin the kernel binder_ref slot
        // here, under the lock, so that:
        //   - the pin is observable in the kernel before P2 runs any
        //     transaction on this handle (BC_ACQUIRE in P3 cannot
        //     race against a freed binder_ref slot);
        //   - concurrent slow paths cannot both observe "absent" and
        //     thus collapse onto the (CaseA, Some(_)) race in P3
        //     instead of producing two true case-(a) commits.
        //
        // BC_INCREFS + flush_commands inside the lock is safe —
        // talk_with_driver(false) sets read_size = 0 so no BR_* are
        // dispatched, including BR_DEAD_BINDER.
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

    /// Slow-path phase 2: lock-released IPC.
    ///
    /// Performs `ping_binder(0)` for `handle == 0 && sdk_at_least(30)`
    /// and, for [`SlowPathPlan::CaseA`], `query_interface(handle)`.
    /// On any IPC failure, undoes our case (a) pin (if owned) before
    /// propagating the error.
    fn slow_path_p2(&self, handle: u32, plan: SlowPathPlan) -> Result<SlowPathReady> {
        // P2 entry hook (test-only). Used by the same-thread deadlock
        // regression test to fire `send_obituary_for_handle(handle)`
        // from inside the slow path while the `handle_to_proxy` lock
        // is released — the exact scenario that used to deadlock under
        // the monolithic pre-fix slow path.
        #[cfg(test)]
        slow_path_p2_test_hook(handle);

        if handle == 0 && crate::sdk_at_least(30) {
            // RAII restore so a ping failure can't leak
            // CallRestriction::None into later calls on this thread.
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

    /// Slow-path phase 3: short write-lock window that re-checks
    /// races spawned during P2 and commits the cache entry.
    ///
    /// Race resolution table (P2 ready × cache state at P3):
    ///
    /// | ready  | cached at P3 | action                                              |
    /// |--------|--------------|-----------------------------------------------------|
    /// | (any)  | live entry   | drop our work; if CaseA, undo our pin               |
    /// | CaseA  | None         | standard commit; new generation                     |
    /// | CaseA  | Some(_)      | undo our pin; commit using cached desc/gen          |
    /// | CaseB  | None         | DeadObject (cache pin gone — BC_ACQUIRE unsafe)     |
    /// | CaseB  | Some, gen=   | resurrect under same generation                     |
    /// | CaseB  | Some, gen≠   | adopt new entry's desc/gen                          |
    fn slow_path_p3(
        &self,
        handle: u32,
        stability: Stability,
        ready: SlowPathReady,
    ) -> Result<SIBinder> {
        let mut handle_to_proxy = self
            .handle_to_proxy
            .write()
            .expect("Handle to proxy lock poisoned");

        // Re-check (c): a concurrent slow path completed during our
        // P2 IPC. Our work is redundant.
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
                // Cross-thread race: while P2 ran, another thread
                // (T2) completed a case (a) for this handle and then
                // dropped its Arc, leaving the entry present + weak
                // dead. T2's BC_INCREFS pin is owned by the cache
                // entry; ours is spare. Undo ours so the
                // "entry-1 ↔ pin-1" invariant holds, then resurrect
                // under T2's descriptor/generation.
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
                // CaseB confirmed: same entry, same generation. The
                // cache pin from first insertion is still active.
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
                // Generation differs: original entry was obituary'd
                // and a new case (a) installed a fresh slot during
                // P2. Drop our CaseB plan and follow the new entry.
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
                // Cache entry vanished mid-flight (obituary). The
                // pin we relied on for BC_ACQUIRE may be gone.
                // Surfacing DeadObject is safer than racing
                // BC_ACQUIRE against a freed binder_ref slot — the
                // user's contract is to re-resolve through service
                // manager, identical to BR_DEAD_BINDER recovery.
                Err(StatusCode::DeadObject)
            }
        }
    }

    /// Snapshot the cache entry's generation for `handle`, if present.
    ///
    /// Called from `SIBinder::downgrade` when constructing a proxy
    /// `WIBinder` so the resulting weak reference carries the
    /// generation it observed at construction time. A subsequent
    /// `WIBinder::upgrade` rejects (returns `DeadObject`) if the live
    /// entry's generation differs — i.e. the original binder_node was
    /// obituary'd and the same handle id was later recycled to a
    /// different node.
    pub(crate) fn cache_generation_for(&self, handle: u32) -> Option<u64> {
        self.handle_to_proxy
            .read()
            .expect("Handle to proxy lock poisoned")
            .get(&handle)
            .map(|e| e.generation)
    }

    /// Resurrection-only proxy lookup. Companion to
    /// `strong_proxy_for_handle_stability` but **never** enters
    /// case (a) (fresh `BC_INCREFS` pin) — if no cache entry exists for
    /// `handle`, returns `Err(StatusCode::DeadObject)`.
    ///
    /// Used by `WIBinder::upgrade` to promote a weak proxy reference to
    /// a strong one without reissuing the cache pin. Three outcomes:
    ///
    ///   - `expected_generation` mismatch ⟹ the original binder_node
    ///     was obituary'd and the handle id was recycled. Return
    ///     `DeadObject`.
    ///   - cache entry alive (some other thread/holder has a strong
    ///     `Arc<ProxyHandle>`) ⟹ reuse it (analogous to case (c)).
    ///   - cache entry's `weak` is dangling ⟹ resurrection (case (b)):
    ///     allocate a fresh `Arc<ProxyHandle>`, issue `BC_ACQUIRE`. The
    ///     cache pin invariant guarantees the kernel `binder_ref` slot
    ///     is still alive, so this `BC_ACQUIRE` cannot race against a
    ///     freed slot.
    pub(crate) fn resurrect_proxy_for_handle_stability(
        &self,
        handle: u32,
        stability: Stability,
        expected_generation: u64,
    ) -> Result<SIBinder> {
        // Read fast path with generation check.
        if let Some(arc) = self
            .handle_to_proxy
            .read()
            .expect("Handle to proxy lock poisoned")
            .get(&handle)
            .filter(|e| e.generation == expected_generation)
            .and_then(|e| e.weak.upgrade())
        {
            return Ok(SIBinder::from_arc(arc as Arc<dyn IBinder>));
        }

        // Slow path: write lock so insert in case (b) is atomic against
        // concurrent resurrections / lookups.
        let mut handle_to_proxy = self
            .handle_to_proxy
            .write()
            .expect("Handle to proxy lock poisoned");

        let (descriptor, generation) = match handle_to_proxy.get(&handle) {
            None => return Err(StatusCode::DeadObject),
            Some(entry) if entry.generation != expected_generation => {
                return Err(StatusCode::DeadObject);
            }
            Some(entry) => {
                if let Some(arc) = entry.weak.upgrade() {
                    return Ok(SIBinder::from_arc(arc as Arc<dyn IBinder>));
                }
                (entry.descriptor.clone(), entry.generation)
            }
        };

        // Case (b) resurrection. Cache pin (BC_INCREFS) is still active
        // for this entry, so BC_ACQUIRE will succeed.
        let arc = ProxyHandle::new_acquired(handle, descriptor.clone(), stability)?;
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

    /// Phase 1 of obituary teardown: remove the cache entry under write
    /// lock and notify recipients. Phase 2 (BC_DECREFS to release the
    /// cache pin) is performed by `release_obituary_pin`, called from
    /// `thread_state::execute_command`'s BR_DEAD_BINDER arm AFTER
    /// BC_DEAD_BINDER_DONE has been queued.
    ///
    /// # Reentrancy with the slow path
    ///
    /// `wait_for_response`'s catch-all arm dispatches
    /// `BR_DEAD_BINDER` to `execute_command`, which calls this
    /// method on the same thread that issued the originating
    /// transaction — including a thread currently inside
    /// [`Self::strong_proxy_for_handle_stability`]. Under
    /// `std::sync::RwLock`'s non-reentrant write semantics, the
    /// `handle_to_proxy.write()` taken here would deadlock if the
    /// slow path were holding that lock across IPC; the slow path's
    /// P1/P2/P3 split keeps the lock released during all IPC for
    /// exactly this reason.
    ///
    /// # Borrow discipline (R1)
    ///
    /// Must be called with NO `THREAD_STATE` or `BINDER_DEREFS` borrow
    /// held — `arc.send_obituary` invokes user
    /// `DeathRecipient::binder_died` callbacks, which can issue nested
    /// binder calls. See [`thread_state`](super::thread_state) module doc.
    pub(crate) fn send_obituary_for_handle(&self, handle: u32) -> Result<()> {
        let entry = {
            let mut handle_to_proxy = self
                .handle_to_proxy
                .write()
                .expect("Handle to proxy lock poisoned");
            handle_to_proxy.remove(&handle)
        };

        if let Some(entry) = entry {
            // The entry's `weak` may or may not still upgrade. Recipients
            // are only meaningful while a live proxy exists, since
            // `link_to_death` requires an `Arc<ProxyHandle>` to hand out
            // recipients. If the Arc is gone, no recipients can be
            // pending and we simply drop the cache entry.
            if let Some(arc) = entry.weak.upgrade() {
                let sibinder = SIBinder::from_arc(arc.clone() as Arc<dyn IBinder>);
                let who = SIBinder::downgrade(&sibinder);
                arc.send_obituary(&who)?;
            } else {
                log::trace!("Object for handle {handle} already destroyed at obituary time");
            }
        } else {
            log::trace!("Handle {handle} was not in cache during obituary");
        }

        Ok(())
    }

    /// Phase 2 of obituary teardown: release the cache pin
    /// (BC_DECREFS). Called from `thread_state::execute_command`'s
    /// BR_DEAD_BINDER arm AFTER `BC_DEAD_BINDER_DONE` has been queued
    /// in this thread's out-parcel.
    ///
    /// `flush_commands()` here commits BC_DEAD_BINDER_DONE plus any
    /// BC_RELEASEs queued IN THIS THREAD before BC_DECREFS reaches the
    /// kernel. It does NOT drain other threads' out-parcels —
    /// concurrent BC_RELEASEs from Drops on other threads can still
    /// arrive after our BC_DECREFS. The kernel rejects those with
    /// -EINVAL and a dmesg log entry, which is acceptable; strict
    /// elimination of that window would require global cross-thread
    /// synchronization which isn't worth the cost.
    pub(crate) fn release_obituary_pin(&self, handle: u32) -> Result<()> {
        thread_state::flush_commands()?;
        thread_state::dec_weak_handle(handle)?;
        thread_state::flush_commands()?;
        Ok(())
    }

    /// Publish a native binder into the sidecar table and return its id.
    ///
    /// The id is what `flat_binder_object.binder` will carry under the new
    /// encoding (replacing the data half of the old fat-pointer pair).
    ///
    /// Dedup is by `Arc::ptr_eq` against existing `binder_pin` entries —
    /// publishing the same `Arc` twice returns the same id without any
    /// counter side effects, matching Android's behavior where a single
    /// `binder_node` is allocated per `weakref_type*` regardless of how
    /// many times it is sent.
    ///
    /// On a fresh insert, `RefCounter.strong` is driven 0→1 via
    /// `SIBinder::from_arc` (which calls `inc_strong` once on the inner
    /// trait object) and `RefCounter.weak` is driven 0→1 via an explicit
    /// `arc.inc_weak(&dummy_wi)` call. Both counters stay at the binary
    /// "alive" floor while the entry exists; user-side strong/weak
    /// increments ride on top and never trigger the count→0 closure path
    /// because the table-controlled +1 keeps the count above zero.
    ///
    /// `publish_count` starts at 0; the immediately-following
    /// `Parcel::write_object` → `flat_binder_object::acquire` brings it
    /// to 1. The single-statement window between this method returning
    /// and the first `acquire` is the only leak path under
    /// `Parcel::write_aligned` panics (typically OOM) — see plan §5
    /// "From<&SIBinder> returning before acquire() is called".
    pub(crate) fn publish_native(&self, arc: Arc<dyn IBinder>) -> u64 {
        // Single write lock for dedup + insert: a read-then-write split
        // would race two concurrent publishes of the same Arc into
        // duplicate entries, breaking the dedup invariant.
        let mut map = self
            .published_natives
            .write()
            .expect("Published natives lock poisoned");
        for (existing_id, entry) in map.iter() {
            if Arc::ptr_eq(entry.binder_pin.as_arc(), &arc) {
                return *existing_id;
            }
        }
        // Drive RefCounter.strong 0→1 via SIBinder::from_arc → inc_strong.
        let binder_pin = SIBinder::from_arc(Arc::clone(&arc));
        // Drive RefCounter.weak 0→1 via explicit inc_weak. The dummy
        // WIBinder satisfies the trait signature; native::inc_weak
        // ignores it. WIBinder has no custom Drop impl, so dropping
        // dummy_wi at scope end only decrements the std::sync::Weak's
        // own reference count — RefCounter.weak is untouched.
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
            },
        );
        id
    }

    /// `flat_binder_object::acquire` BINDER_TYPE_BINDER arm.
    ///
    /// Returns `false` if `id` is unknown — should not happen in practice
    /// because every `acquire` is paired with a `From<&SIBinder>` that
    /// just inserted the entry (or a buffer-clone via
    /// `Parcel::append_from` whose source already holds an entry).
    /// Callers `debug_assert!` in dev builds and `log::error!` + skip in
    /// production.
    pub(crate) fn incref_publish(&self, id: u64) -> bool {
        let mut map = self
            .published_natives
            .write()
            .expect("Published natives lock poisoned");
        match map.get_mut(&id) {
            Some(entry) => {
                entry.publish_count += 1;
                true
            }
            None => false,
        }
    }

    /// `flat_binder_object::release` BINDER_TYPE_BINDER arm.
    ///
    /// Decrements `publish_count`. If both `publish_count` and
    /// `kernel_refs` reach zero, the entry is removed (drives
    /// `RefCounter.strong` / `RefCounter.weak` 1→0, drops the
    /// `binder_pin` SIBinder). Returns `false` if `id` is unknown.
    pub(crate) fn decref_publish(&self, id: u64) -> bool {
        let trigger_remove = {
            let mut map = self
                .published_natives
                .write()
                .expect("Published natives lock poisoned");
            match map.get_mut(&id) {
                Some(entry) => {
                    // `saturating_sub` is the production safety net for
                    // an unpaired `release` (which would otherwise wrap
                    // u32 → 4 billion); the `debug_assert` makes the
                    // unpaired call loud during dev/CI so a future
                    // `acquire`/`release` pairing bug doesn't slip
                    // through silently.
                    debug_assert!(
                        entry.publish_count > 0,
                        "decref_publish on id {id} with publish_count == 0 \
                         (unpaired release; check From<&SIBinder> ↔ \
                         flat_binder_object::release pairing)"
                    );
                    entry.publish_count = entry.publish_count.saturating_sub(1);
                    entry.publish_count == 0 && entry.kernel_refs == 0
                }
                None => return false,
            }
        };
        if trigger_remove {
            self.remove_entry_if_zero(id);
        }
        true
    }

    /// `BR_INCREFS` / `BR_ACQUIRE` / `BR_ATTEMPT_ACQUIRE` arms: bump
    /// `kernel_refs`. Returns `Some(arc)` while the entry is alive
    /// (caller may dispatch methods on the arc); `None` if `id` is
    /// unknown (kernel invariant violation in `BR_INCREFS` / `BR_ACQUIRE`,
    /// expected race for `BR_ATTEMPT_ACQUIRE`).
    pub(crate) fn ref_native_kernel(&self, id: u64) -> Option<Arc<dyn IBinder>> {
        let mut map = self
            .published_natives
            .write()
            .expect("Published natives lock poisoned");
        let entry = map.get_mut(&id)?;
        entry.kernel_refs += 1;
        Some(Arc::clone(entry.binder_pin.as_arc()))
    }

    /// `BR_RELEASE` / `BR_DECREFS` arms (deferred via
    /// `pending_*_derefs`): decrement `kernel_refs`. If both
    /// `publish_count` and `kernel_refs` reach zero, the entry is
    /// removed (RefCounter floor torn down, Arc dropped). Returns
    /// `Some(arc)` while the entry was still present pre-removal;
    /// `None` if `id` is unknown.
    pub(crate) fn deref_native_kernel(&self, id: u64) -> Option<Arc<dyn IBinder>> {
        let (arc, trigger_remove) = {
            let mut map = self
                .published_natives
                .write()
                .expect("Published natives lock poisoned");
            let entry = map.get_mut(&id)?;
            // `saturating_sub` clamps at 0 under a cross-thread race
            // where `BR_DECREFS` is processed before its matching
            // `BR_INCREFS`. The kernel queues `BR_INCREFS` /
            // `BR_DECREFS` to `proc->todo` (process-wide FIFO), so
            // distinct binder threads can pop the matching pair in
            // FIFO order but dispatch out of order if the
            // `BR_INCREFS` thread is preempted before reaching
            // `ref_native_kernel`. Under that race the late
            // `BR_INCREFS` will bump `kernel_refs` from 0 to 1 (we
            // missed the dec that should have followed). Each race
            // occurrence on a given id adds one to `kernel_refs`'s
            // over-count vs the kernel's true ref count; the
            // accumulation is **unbounded** over the binder's
            // lifetime if races recur, leaving the entry permanently
            // stranded with `kernel_refs >= 1` even after the kernel
            // has fully released. Bounded only by "one entry per
            // long-lived published binder that ever raced." We
            // accept this over a `debug_assert` panic: the race is a
            // property of kernel scheduling, not our bookkeeping, so
            // panicking would fail CI on a legitimate interleaving.
            // (The OLD fat-pointer encoding hit the same race but
            // masked it via `RefCounter`'s `INITIAL_STRONG_VALUE`
            // pattern, which self-corrects the count to its initial
            // value but silently skips the first/last-ref closures —
            // equivalently broken in semantics, just lower-noise.)
            // See plan §5 #7. A future change could move to a
            // signed counter + dual-direction removal trigger to
            // bound the drift, but that introduces premature-removal
            // hazards in multi-pair scenarios; left as a follow-up.
            entry.kernel_refs = entry.kernel_refs.saturating_sub(1);
            let arc = Arc::clone(entry.binder_pin.as_arc());
            let trigger = entry.publish_count == 0 && entry.kernel_refs == 0;
            (arc, trigger)
        };
        if trigger_remove {
            self.remove_entry_if_zero(id);
        }
        Some(arc)
    }

    /// `BR_TRANSACTION` and round-trip `BINDER_TYPE_BINDER` receive
    /// path: read-only lookup. Does not change counts. Returns `None`
    /// if the id is unknown.
    pub(crate) fn lookup_native(&self, id: u64) -> Option<Arc<dyn IBinder>> {
        let map = self
            .published_natives
            .read()
            .expect("Published natives lock poisoned");
        map.get(&id).map(|e| Arc::clone(e.binder_pin.as_arc()))
    }

    /// Remove the entry for `id` and tear down the RefCounter floor —
    /// but only if both `publish_count` and `kernel_refs` are still zero
    /// when re-checked under the write lock. The two-phase pattern
    /// (counter-mutate under lock, release lock, re-acquire for
    /// removal) is required because `SIBinder::Drop` calls
    /// `dec_strong(None)` which may run user destructor code (via
    /// `Inner<T>::drop`) that itself calls back into `ProcessState` —
    /// holding the write lock across that path would deadlock.
    ///
    /// The re-check under the new lock makes the two-phase pattern
    /// race-free: if a concurrent `BR_INCREFS` / `From<&SIBinder>`
    /// bumped a counter back above zero between phases, we abort the
    /// removal. Same shape as the proxy-side `CacheEntry` removal in
    /// `send_obituary_for_handle`.
    fn remove_entry_if_zero(&self, id: u64) {
        let entry = {
            let mut map = self
                .published_natives
                .write()
                .expect("Published natives lock poisoned");
            let needs_remove = map
                .get(&id)
                .map(|e| e.publish_count == 0 && e.kernel_refs == 0)
                .unwrap_or(false);
            if !needs_remove {
                return;
            }
            map.remove(&id).expect("just observed Some")
        };
        // Symmetric with publish_native: dec_weak first (no destructor
        // side effect — `Inner<T>::dec_weak` only touches RefCounter.weak),
        // then drop binder_pin which fires SIBinder::Drop →
        // dec_strong(None) → RefCounter.strong 1→0. The Arc inside
        // binder_pin is the canonical strong reference; if no user-side
        // SIBinder clones survive, that drop also takes the Arc strong
        // count to zero, triggering Inner<T>::drop CLEANLY — kernel has
        // guaranteed no further BR_* will reference this id (kernel_refs
        // was 0 to reach this branch).
        let arc_for_weak = Arc::clone(entry.binder_pin.as_arc());
        let _ = arc_for_weak.dec_weak();
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

    pub fn start_thread_pool() {
        let this = Self::as_self();
        if this
            .thread_pool_started
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            if this.max_threads == 0 {
                log::warn!("Extra binder thread started, but 0 threads requested.\nDo not use *start_thread_pool when zero threads are requested.");
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
            let _ = thread::Builder::new()
                .name(name)
                .spawn(move || thread_state::join_thread_pool(is_main));

            self.kernel_started_threads.fetch_add(1, Ordering::SeqCst);
        }
        // TODO: if startThreadPool is called on another thread after the process
        // starts up, the kernel might think that it already requested those
        // binder threads, and additional won't be started. This is likely to
        // cause deadlocks, and it will also cause getThreadPoolMaxTotalThreadCount
        // to return too high of a value.
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

impl Drop for ProcessState {
    fn drop(self: &mut ProcessState) {
        let mmap = self.mmap.read().expect("Mmap lock poisoned");
        // SAFETY: `mmap.ptr`/`mmap.size` are exactly the address and length
        // returned by the `mmap` call in `ProcessState::new`. This runs only
        // in `Drop`, so the mapping is still live and is unmapped exactly
        // once; no references into the region outlive `ProcessState`.
        unsafe {
            rustix::mm::munmap(mmap.ptr, mmap.size).expect("Failed to unmap memory");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared init + invariant checks for the two tests that assert a
    /// freshly-initialized `ProcessState`. Deliberately NOT `#[serial]`:
    /// both callers already run inside the `binder` serial section, so
    /// keeping this a plain fn avoids depending on serial_test's lock
    /// being reentrant for a same-thread nested `#[serial]` call.
    fn assert_process_state_initialized() {
        let process = ProcessState::init_default().expect("init_default");
        assert_eq!(process.max_threads, DEFAULT_MAX_BINDER_THREADS);
        assert_eq!(
            process.driver_name,
            PathBuf::from(crate::DEFAULT_BINDER_PATH)
        );
    }

    #[test]
    #[serial_test::serial(binder)]
    fn test_process_state() {
        assert_process_state_initialized();
    }

    #[test]
    #[serial_test::serial(binder)]
    fn test_process_state_context_object() {
        let process = ProcessState::init_default().expect("init_default");
        assert!(process.context_object().is_ok());
    }

    #[test]
    #[serial_test::serial(binder)]
    fn test_process_state_strong_proxy_for_handle() {
        let process = ProcessState::init_default().expect("init_default");
        assert!(process.strong_proxy_for_handle(0).is_ok());
    }

    /// N threads racing on the same uncached handle (service manager =
    /// 0) must converge on a single cache entry and a single `Arc`
    /// identity. Exercises the lock-decoupled three-phase slow path's
    /// race-resolution table — at most one P3 winner installs the
    /// entry, every other thread either short-circuits in P1's case
    /// (c) re-check, P3's case (c) re-check, or P3's
    /// `(CaseA, Some(_))` cross-thread arm.
    #[test]
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

    /// Drop all live `Arc<ProxyHandle>` for handle 0 so the cache
    /// entry's `weak` is dangling, then race N threads through the
    /// resurrection path. Each thread observes case (b) in P1 (entry
    /// present, weak dead) and races to commit in P3 — only one
    /// winner; the rest fall through P3's case (c) re-check and reuse
    /// the winner's Arc. Verifies the case (b) generation-preservation
    /// invariant survives concurrent resurrection.
    #[test]
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
        // Yield so any other Arc borrowers (e.g. `context_manager`
        // cache) settle. In a clean test process there are no other
        // strong refs to handle 0 by this point.
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
        // Generation preserved (case (b) reuses the existing entry's
        // generation; a fresh case (a) would have allocated a new one).
        assert_eq!(
            ProcessState::as_self().cache_generation_for(0),
            Some(initial_gen),
            "case (b) resurrection must preserve the entry's generation"
        );
    }

    /// Plan §5.2 — same-thread re-entrant obituary regression guard.
    ///
    /// Reproduces the exact deadlock the P1/P2/P3 split closes:
    /// while the slow path is mid-flight, a `BR_DEAD_BINDER` for the
    /// same handle dispatches `send_obituary_for_handle` on the
    /// *same* thread, which re-acquires `handle_to_proxy.write()`.
    /// Under the pre-fix monolithic slow path that lock was already
    /// held by this thread → `std::sync::RwLock`'s non-reentrant
    /// write semantics → hang. Under the post-fix split P1 has
    /// released the lock by the time the obituary fires, so the
    /// re-acquisition succeeds.
    ///
    /// The simulation drives the slow path on a worker thread that
    /// installs a `slow_path_p2` cfg(test) hook calling
    /// `send_obituary_for_handle` from the same thread (fired the
    /// instant P1 releases the lock and before P2 enters IPC).
    /// Driving the obituary from the actual binder driver would
    /// require crashing a service mid-transact — too brittle for a
    /// unit test, and the lock semantics being tested are
    /// driver-independent.
    ///
    /// Wallclock-bounded so a regression manifests as a CI timeout
    /// failure rather than an indefinite hang. The `fired` flag
    /// asserts the hook actually ran — required because the
    /// process-wide singleton `ProcessState` is shared with other
    /// parallel tests, and a sibling test holding an `Arc` for
    /// handle 0 could keep the cache `Weak` upgradeable, causing P1
    /// to short-circuit at case (c) and the hook to never fire
    /// (vacuous pass).
    #[test]
    #[serial_test::serial(binder)]
    fn test_strong_proxy_under_same_thread_dead_binder_no_deadlock() {
        let process = ProcessState::init_default().expect("init_default");

        // Seed handle 0 (service manager) into the cache, then drop
        // so the next lookup hits case (b) — entry present, weak
        // dead. Case (b) lets us exercise the lock pattern without
        // having to issue a fresh BC_INCREFS that the kernel might
        // reject mid-test.
        let seed = process
            .strong_proxy_for_handle(0)
            .expect("seed strong_proxy_for_handle(0) must succeed");
        drop(seed);

        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_w = std::sync::Arc::clone(&fired);

        let (tx, rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            // Hook is thread-local: install on the worker so the
            // injected obituary fires on the same thread that is
            // running strong_proxy_for_handle.
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

        // Wallclock bound: regression in the lock structure manifests
        // as an indefinite hang here.
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

        // Either outcome is acceptable — what we are guarding
        // against is the deadlock, not the resolution. After the
        // obituary, P3's (CaseB, None) arm normally returns
        // DeadObject; a parallel resurrection might also produce a
        // live Arc.
        match result {
            Ok(_arc) => {}
            Err(StatusCode::DeadObject) => {}
            Err(other) => panic!("unexpected slow-path result: {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial(binder)]
    fn test_process_state_disable_background_scheduling() {
        let process = ProcessState::init_default().expect("init_default");
        process.disable_background_scheduling(true);
        assert!(process.background_scheduling_disabled());
    }

    #[test]
    #[serial_test::serial(binder)]
    fn test_process_state_start_thread_pool() {
        // `kernel_started_threads` is a process-wide `AtomicUsize` that
        // also tracks kernel-driven `BR_SPAWN_LOOPER` events from prior
        // tests in the same process, so we can't assert an absolute
        // value of 1. Instead we capture the pre-state and assert the
        // contract `start_thread_pool` actually promises: on the first
        // call it flips `thread_pool_started` and spawns exactly one
        // pooled thread; subsequent calls are no-ops.
        assert_process_state_initialized();
        let process = ProcessState::as_self();
        let was_started = process.thread_pool_started.load(Ordering::SeqCst);
        let before = process.kernel_started_threads.load(Ordering::SeqCst);
        ProcessState::start_thread_pool();
        assert!(process.thread_pool_started.load(Ordering::SeqCst));
        let after = process.kernel_started_threads.load(Ordering::SeqCst);
        if was_started {
            assert_eq!(after, before);
        } else {
            assert_eq!(after, before + 1);
        }
    }

    /// Minimal `IBinder` impl for the `published_natives` bookkeeping
    /// tests below. Ref-count methods are no-ops — these tests exercise
    /// the table's accounting (publish_count / kernel_refs and entry
    /// removal-on-zero) without relying on `RefCounter` state.
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

    /// End-to-end of the table-controlled lifecycle that closes the UAF
    /// window. Mirrors plan §4 "test_native_uaf_window_closed":
    ///
    ///   1. publish a native binder → entry created, `publish_count = 0`,
    ///      `kernel_refs = 0`, RefCounter floor armed.
    ///   2. `incref_publish` (mirrors the first `acquire()` that
    ///      `Parcel::write_object` would call): `publish_count = 1`.
    ///   3. drop the local user-side strong ref (under the OLD encoding
    ///      this could dangle `Inner<T>` once the kernel finished
    ///      releasing; under the new model the table's `binder_pin`
    ///      keeps the canonical Arc alive).
    ///   4. simulate `BR_INCREFS` / `BR_ACQUIRE` / `BR_RELEASE` /
    ///      `BR_DECREFS` arrival as pure id-bookkeeping.
    ///   5. mirror `Parcel::release_objects` → `release()` →
    ///      `decref_publish`: `publish_count = 0`. Now both counters
    ///      are zero and the entry is removed → `lookup_native` returns
    ///      `None`.
    #[test]
    #[serial_test::serial(binder)]
    fn test_native_uaf_window_closed() {
        let process = ProcessState::init_default().expect("init_default");
        let arc: Arc<dyn IBinder> = Arc::new(MockNative);

        let id = process.publish_native(Arc::clone(&arc));
        assert!(
            process.incref_publish(id),
            "incref on freshly published id must succeed"
        );

        // Drop the user-side Arc clone; only the table's binder_pin
        // SIBinder keeps the inner Arc alive now.
        drop(arc);

        // BR_INCREFS / BR_ACQUIRE: kernel_refs goes 0→1→2.
        assert!(process.ref_native_kernel(id).is_some());
        assert!(process.ref_native_kernel(id).is_some());
        // BR_RELEASE: kernel_refs 2→1. Entry still alive
        // (publish_count=1, kernel_refs=1).
        assert!(process.deref_native_kernel(id).is_some());
        assert!(
            process.lookup_native(id).is_some(),
            "entry must remain while publish_count > 0"
        );

        // Parcel::release_objects → release() → decref_publish:
        // publish_count 1→0; kernel_refs still 1.
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

    /// Two `publish_native` calls with the same `Arc<dyn IBinder>`
    /// dedup to the same id. Driving each parcel slot's
    /// `acquire`/`release` independently keeps the entry alive until
    /// the last `release` fires.
    #[test]
    #[serial_test::serial(binder)]
    fn test_native_dedup_same_arc() {
        let process = ProcessState::init_default().expect("init_default");
        let arc: Arc<dyn IBinder> = Arc::new(MockNative);

        let id1 = process.publish_native(Arc::clone(&arc));
        let id2 = process.publish_native(Arc::clone(&arc));
        assert_eq!(id1, id2, "publishing the same Arc twice must dedup");

        // Two parcel slots reference the same id — `acquire` runs
        // twice, `release` must run twice before the entry can drop.
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

    /// Distinct `Arc`s get distinct ids (no false-positive dedup via
    /// e.g. `MockNative` being a unit struct — `Arc::ptr_eq` keys on
    /// allocation, not type).
    #[test]
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

    /// `lookup_native` is read-only — does not change `publish_count`
    /// or `kernel_refs`. Exercises the BR_TRANSACTION /
    /// `deserialize_option` round-trip path where the kernel routes a
    /// previously-published binder back to its publisher.
    #[test]
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
