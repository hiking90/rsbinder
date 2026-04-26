// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs::File;
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
pub(crate) struct CacheEntry {
    pub(crate) weak: sync::Weak<ProxyHandle>,
    pub(crate) descriptor: String,
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
unsafe impl Sync for MemoryMap {}
unsafe impl Send for MemoryMap {}

pub struct ProcessState {
    max_threads: u32,
    driver_name: PathBuf,
    driver: Arc<File>,
    mmap: RwLock<MemoryMap>,
    context_manager: RwLock<Option<SIBinder>>,
    handle_to_proxy: RwLock<HashMap<u32, CacheEntry>>,
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

        let mmap = unsafe {
            // let vm_start = nix::sys::mman::mmap(None,
            //     vm_size,
            //     nix::sys::mman::ProtFlags::PROT_READ,
            //     nix::sys::mman::MapFlags::MAP_PRIVATE | nix::sys::mman::MapFlags::MAP_NORESERVE,
            //     &driver,
            //     0)?;

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
    pub fn init(driver_name: &str, max_threads: u32) -> &'static ProcessState {
        // TODO: panic! is not good. It should return Result.
        // But, get_or_try_init is not stable yet.
        Self::instance().get_or_init(|| match Self::inner_init(driver_name, max_threads) {
            Ok(instance) => instance,
            Err(e) => {
                panic!("Error in init(): {e}");
            }
        })
    }

    /// Initialize ProcessState with default binder path and max threads.
    /// The meaning of zero max threads is to use the default value. It is dependent on the kernel.
    /// DEFAULT_BINDER_PATH is "/dev/binderfs/binder".
    pub fn init_default() -> &'static ProcessState {
        Self::init(crate::DEFAULT_BINDER_PATH, 0)
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

        // Write-lock slow path. Three sub-cases distinguished after taking
        // the lock:
        //   (a) entry absent          → BC_INCREFS pin + flush + query +
        //                                 BC_ACQUIRE + insert
        //   (b) entry present, dead   → reuse cached descriptor + BC_ACQUIRE
        //                                 (cache pin still active)
        //   (c) entry present, alive  → another thread won the race;
        //                                 return its Arc
        let mut handle_to_proxy = self
            .handle_to_proxy
            .write()
            .expect("Handle to proxy lock poisoned");

        // Sub-case (c): another thread inserted/upgraded between our
        // read-fast-path miss and write-lock acquisition.
        if let Some(arc) = handle_to_proxy.get(&handle).and_then(|e| e.weak.upgrade()) {
            return Ok(SIBinder::from_arc(arc));
        }

        // Distinguish (a) vs (b). On (b) the entry is present (with a
        // dangling weak) and we reuse the cached descriptor; the cache
        // pin (BC_INCREFS) issued at first insertion is still active so
        // BC_ACQUIRE below will succeed. On (a) the entry is absent — we
        // pin first, then query, then insert.
        let cached_descriptor = handle_to_proxy.get(&handle).map(|e| e.descriptor.clone());

        if handle == 0 {
            let original_call_restriction = thread_state::call_restriction();
            thread_state::set_call_restriction(CallRestriction::None);
            thread_state::ping_binder(handle)?;
            thread_state::set_call_restriction(original_call_restriction);
        }

        let descriptor = match cached_descriptor {
            Some(desc) => {
                // Sub-case (b): entry present, dead Arc. Skip BC_INCREFS
                // (would double-pin) and skip INTERFACE_TRANSACTION
                // (descriptor immutable for the binder_ref slot's
                // lifetime).
                desc
            }
            None => {
                // Sub-case (a): entry absent. Pin the kernel binder_ref
                // slot before the (potentially failing) descriptor query.
                //
                // Step 1: queue BC_INCREFS on this thread's out-parcel.
                thread_state::inc_weak_handle(handle)?;
                // Step 2: flush so the kernel observes BC_INCREFS now.
                // If the kernel has already freed binder_ref(handle) —
                // recycled handle id, never-valid id, etc. — the ioctl
                // returns -EINVAL and we propagate `DeadObject` without
                // touching the cache. Subsequent re-resolution through
                // service manager is the user's contract (same as after
                // BR_DEAD_BINDER).
                if let Err(err) = thread_state::flush_commands() {
                    log::warn!(
                        "BC_INCREFS for handle {handle} failed at flush: {err:?}; \
                         handle is no longer valid in the kernel"
                    );
                    return Err(StatusCode::DeadObject);
                }
                // Step 3: pin is live in the kernel. Query the interface
                // descriptor. On failure we MUST undo the pin so the
                // kernel does not leak a binder_ref slot.
                match thread_state::query_interface(handle) {
                    Ok(s) => s,
                    Err(err) => {
                        undo_case_a_pin(handle);
                        return Err(err);
                    }
                }
            }
        };

        // Allocate ProxyHandle + acquire kernel strong ref. The cache pin
        // (already held for case (b), or freshly issued+flushed above for
        // case (a)) guarantees binder_ref(handle) is alive at this moment,
        // so BC_ACQUIRE succeeds.
        //
        // If `new_acquired` fails we MUST undo the case (a) pin (case (b)'s
        // pin was issued at first insertion and is still owned by the
        // existing cache entry, which we leave intact). Distinguishing the
        // two cases is exactly `!handle_to_proxy.contains_key(&handle)` —
        // case (a) entered with no entry, case (b) entered with one.
        let arc = match ProxyHandle::new_acquired(handle, descriptor.clone(), stability) {
            Ok(arc) => arc,
            Err(err) => {
                if !handle_to_proxy.contains_key(&handle) {
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
            },
        );
        Ok(SIBinder::from_arc(arc as Arc<dyn IBinder>))
    }

    /// Phase 1 of obituary teardown: remove the cache entry under write
    /// lock and notify recipients. Phase 2 (BC_DECREFS to release the
    /// cache pin) is performed by `release_obituary_pin`, called from
    /// `thread_state::execute_command`'s BR_DEAD_BINDER arm AFTER
    /// BC_DEAD_BINDER_DONE has been queued.
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
        unsafe {
            rustix::mm::munmap(mmap.ptr, mmap.size).expect("Failed to unmap memory");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_state() {
        let process = ProcessState::init_default();
        assert_eq!(process.max_threads, DEFAULT_MAX_BINDER_THREADS);
        assert_eq!(
            process.driver_name,
            PathBuf::from(crate::DEFAULT_BINDER_PATH)
        );
    }

    #[test]
    fn test_process_state_context_object() {
        let process = ProcessState::init_default();
        assert!(process.context_object().is_ok());
    }

    #[test]
    fn test_process_state_strong_proxy_for_handle() {
        let process = ProcessState::init_default();
        assert!(process.strong_proxy_for_handle(0).is_ok());
    }

    #[test]
    fn test_process_state_disable_background_scheduling() {
        let process = ProcessState::init_default();
        process.disable_background_scheduling(true);
        assert!(process.background_scheduling_disabled());
    }

    #[test]
    fn test_process_state_start_thread_pool() {
        test_process_state();
        ProcessState::start_thread_pool();
        assert_eq!(
            ProcessState::as_self()
                .kernel_started_threads
                .load(Ordering::SeqCst),
            1
        );
    }
}
