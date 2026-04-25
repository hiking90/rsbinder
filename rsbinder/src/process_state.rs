// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs::File;
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::thread;

use crate::{binder::*, error::*, proxy::*, sys::binder, thread_state};

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
    handle_to_proxy: RwLock<HashMap<u32, WIBinder>>,
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
        // Double-Checked Locking Pattern is used.
        if let Some(weak) = self
            .handle_to_proxy
            .read()
            .expect("Handle to proxy lock poisoned")
            .get(&handle)
        {
            return weak.upgrade();
        }

        let mut handle_to_proxy = self
            .handle_to_proxy
            .write()
            .expect("Handle to proxy lock poisoned");
        if let Some(weak) = handle_to_proxy.get(&handle) {
            return weak.upgrade();
        }

        if handle == 0 {
            let original_call_restriction = thread_state::call_restriction();
            thread_state::set_call_restriction(CallRestriction::None);

            thread_state::ping_binder(handle)?;

            thread_state::set_call_restriction(original_call_restriction);
        }

        // Hold the write lock across `query_interface` so that only one
        // thread is performing the INTERFACE_TRANSACTION for `handle` at a
        // time — under heavy parallel lookup load this naturally rate-limits
        // the receiver's binder thread pool and avoids exhausting it with
        // 100+ redundant queries for the same handle.
        //
        // Crucially do NOT silently swallow a transaction failure into an
        // empty descriptor: doing so used to insert a permanent
        // empty-descriptor entry into the cache, after which every
        // `FromIBinder::try_from` for that handle returned `BadType` for
        // the lifetime of the process (root cause of the intermittent
        // CI flake on `test_renamed_interface_*` and similar tests).
        // Propagate the error instead so the next caller retries.
        let interface = match thread_state::query_interface(handle) {
            Ok(s) => s,
            Err(err) => {
                log::warn!(
                    "query_interface(handle={handle}) failed: {err:?}; \
                     not caching, caller may retry"
                );
                return Err(err);
            }
        };

        let proxy: Arc<dyn IBinder> = ProxyHandle::new(handle, interface, stability);
        let weak = WIBinder::new(proxy)?;

        handle_to_proxy.insert(handle, weak.clone());

        weak.upgrade()
    }

    pub(crate) fn send_obituary_for_handle(&self, handle: u32) -> Result<()> {
        // Extract the weak reference atomically by removing it from the map.
        // This ensures only one thread can retrieve and process this handle's obituary.
        let weak = {
            let mut handle_to_proxy = self
                .handle_to_proxy
                .write()
                .expect("Handle to proxy lock poisoned");
            handle_to_proxy.remove(&handle)
        };
        // Write lock is released here

        // Send obituary notification outside the lock to avoid potential deadlock.
        // If the handle wasn't in the map, this is a no-op.
        if let Some(weak) = weak {
            match weak.upgrade() {
                Ok(strong) => {
                    if let Some(proxy) = strong.as_proxy() {
                        // Send obituary - this is best-effort notification
                        proxy.send_obituary(&weak)?;
                    } else {
                        log::debug!("Handle {} is not a proxy during obituary", handle);
                    }
                }
                Err(_) => {
                    // Weak reference upgrade failed - object already destroyed
                    // This is expected in many cases and not an error
                    log::trace!("Object for handle {} already destroyed", handle);
                }
            }
        } else {
            log::trace!("Handle {} was not in cache during obituary", handle);
        }

        Ok(())
    }

    /// Send `BC_RELEASE` / `BC_DECREFS` to the kernel for `handle` while
    /// holding the cache write lock, then drop the cache entry iff the
    /// proxy is still orphaned (both strong and weak counters at
    /// `INITIAL_STRONG_VALUE`).
    ///
    /// Issue #47's stale-proxy fix needed to evict cache entries whose
    /// user-visible refcounts had returned to "uninitialized." The original
    /// implementation did the count check in the caller (`ProxyHandle::dec_*`)
    /// BEFORE taking the cache lock, and called `dec_strong_handle` /
    /// `dec_weak_handle` (which write `BC_RELEASE` / `BC_DECREFS` to the
    /// out-parcel and flush) outside any lock. Two races followed:
    ///
    ///   1. **Cache aliasing.** A concurrent
    ///      `strong_proxy_for_handle_stability` lookup could inc the strong
    ///      counter between the caller's check and the eventual map mutation,
    ///      after which the cache was emptied even though a freshly-handed-out
    ///      `Strong<...>` still referenced the now-orphaned ProxyHandle. The
    ///      next lookup of the same kernel handle allocated a brand-new
    ///      ProxyHandle, breaking the `Arc::ptr_eq` invariant that AIDL out-
    ///      parameter equality relies on (e.g. `test_nullable_binder_array`).
    ///
    ///   2. **Kernel ref-count race.** Sending `BC_RELEASE` outside the lock
    ///      let the binder driver process the release before a concurrent
    ///      lookup's `BC_INCREFS` arrived; the kernel could free the handle
    ///      and reject the late `BC_INCREFS`, leaving a user-space proxy
    ///      whose handle was no longer valid. Subsequent transactions on
    ///      that proxy returned `DeadObject` even though `Strong<...>` was
    ///      still alive (e.g. intermittent `service.GetOldNameInterface()`
    ///      failures in `test_renamed_interface_*`).
    ///
    /// Funneling the kernel-side ref drop AND the cache eviction through the
    /// cache write lock closes both windows. The lookup path's
    /// `weak.upgrade()` runs entirely inside the cache *read* lock, so any
    /// `inc_strong_handle` (BC_INCREFS) it performs is dispatched to the
    /// kernel before this function can acquire the *write* lock. By the time
    /// we run, the binder driver sees the matched INC/RELEASE pair, the
    /// counts here reflect the post-inc state, and `is_orphan()` correctly
    /// keeps the entry cached for the still-live lookup.
    pub(crate) fn release_strong_under_cache_lock(
        &self,
        handle: u32,
        proxy: &ProxyHandle,
    ) -> Result<()> {
        let mut handle_to_proxy = self
            .handle_to_proxy
            .write()
            .expect("Handle to proxy lock poisoned");

        thread_state::dec_strong_handle(handle)?;

        if proxy.is_orphan() {
            handle_to_proxy.remove(&handle);
            log::trace!("release_strong_under_cache_lock: removed handle {}", handle);
        }
        Ok(())
    }

    pub(crate) fn release_weak_under_cache_lock(
        &self,
        handle: u32,
        proxy: &ProxyHandle,
    ) -> Result<()> {
        let mut handle_to_proxy = self
            .handle_to_proxy
            .write()
            .expect("Handle to proxy lock poisoned");

        thread_state::dec_weak_handle(handle)?;

        if proxy.is_orphan() {
            handle_to_proxy.remove(&handle);
            log::trace!("release_weak_under_cache_lock: removed handle {}", handle);
        }
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
