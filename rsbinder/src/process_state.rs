// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, RwLock, OnceLock};
use std::path::{Path, PathBuf};
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::thread;

use crate::{
    error::*,
    binder::*,
    sys::binder,
    proxy::*,
    thread_state,
};

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
    ptr: *mut std::ffi::c_void,
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
        Self::instance().get().expect("ProcessState is not initialized!")
    }

    pub fn set_call_restriction(&self, call_restriction: CallRestriction) {
        let mut self_call_restriction = self.call_restriction.write().unwrap();
        *self_call_restriction = call_restriction;
    }

    pub(crate) fn call_restriction(&self) -> CallRestriction {
        *self.call_restriction.read().unwrap()
    }

    fn inner_init(driver_name: &str, max_threads: u32) -> std::result::Result<ProcessState, Box<dyn std::error::Error>> {
        let max_threads = if max_threads != 0 && max_threads < DEFAULT_MAX_BINDER_THREADS {
            max_threads
        } else {
            DEFAULT_MAX_BINDER_THREADS
        };

        let driver_name = PathBuf::from(driver_name);

        let driver = open_driver(&driver_name, max_threads)?;

        let vm_size = ((1024 * 1024) - unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) } * 2) as usize;

        let mmap = unsafe {
            let vm_start = libc::mmap(std::ptr::null_mut(),
                vm_size,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE, driver.as_raw_fd(), 0);

            if vm_start == libc::MAP_FAILED {
                return Err(format!("{:?} mmap is failed!", driver_name).into());
            }

            (vm_start, vm_size)
        };

        Ok(ProcessState {
            max_threads,
            driver_name,
            driver: driver.into(),
            mmap: RwLock::new(MemoryMap { ptr: mmap.0, size: mmap.1 }),
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
        Self::instance().get_or_init(|| {
            match Self::inner_init(driver_name, max_threads) {
                Ok(instance) => instance,
                Err(e) => {
                    panic!("Error in init(): {}", e);
                }
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
    pub fn become_context_manager(&self, binder: SIBinder) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let obj = std::mem::MaybeUninit::<binder::flat_binder_object>::zeroed();
        let mut obj = unsafe { obj.assume_init() };
        obj.flags = binder::FLAT_BINDER_FLAG_ACCEPTS_FDS;

        unsafe {
            let driver_fd = self.driver.as_raw_fd();
            if binder::set_context_mgr_ext(driver_fd, &obj).is_err() {
                //     android_errorWriteLog(0x534e4554, "121035042");
                let unused: i32 = 0;
                if let Err(e) = binder::set_context_mgr(driver_fd, &unused) {
                    return Err(format!("Binder ioctl to become context manager failed: {}", e).into());
                }
            }
        }

        *self.context_manager.write().unwrap() = Some(binder);

        Ok(())
    }

    pub(crate) fn context_manager(&self) -> Option<SIBinder> {
        self.context_manager.read().unwrap().clone()
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

    pub(crate) fn strong_proxy_for_handle_stability(&self, handle: u32, stability: Stability) -> Result<SIBinder> {
        if let Some(weak) = self.handle_to_proxy.read().unwrap().get(&handle) {
            return weak.upgrade()
        }

        if handle == 0 {
            let original_call_restriction = thread_state::call_restriction();
            thread_state::set_call_restriction(CallRestriction::None);

            thread_state::ping_binder(handle)?;

            thread_state::set_call_restriction(original_call_restriction);
        }

        let interface = thread_state::query_interface(handle)?;

        let proxy = ProxyHandle::new(handle, &interface, stability);
        let weak = WIBinder::new(Box::new(proxy), &interface)?;

        self.handle_to_proxy.write().unwrap().insert(handle, weak.clone());

        weak.upgrade()
    }

    pub(crate) fn send_obituary_for_handle(&self, handle: u32) -> Result<()> {
        let mut handle_to_proxy = self.handle_to_proxy.write().unwrap();
        if let Some(weak) = handle_to_proxy.get(&handle) {
            weak.upgrade()?.as_proxy().unwrap().send_obituary(weak)?;
        }
        handle_to_proxy.remove(&handle);
        Ok(())
    }

    pub fn disable_background_scheduling(&self, disable: bool) {
        self.disable_background_scheduling.store(disable, Ordering::Relaxed);
    }

    pub fn background_scheduling_disabled(&self) -> bool {
        self.disable_background_scheduling.load(Ordering::Relaxed)
    }

    pub fn driver(&self) -> Arc<File> {
        self.driver.clone()
    }

    pub fn start_thread_pool(&self) {
        if self.thread_pool_started.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            if self.max_threads == 0 {
                log::warn!("Extra binder thread started, but 0 threads requested.\nDo not use *start_thread_pool when zero threads are requested.");
            }
            self.spawn_pooled_thread(true);
        }
    }

    fn make_binder_thread_name(&self) -> String {
        let seq = self.thread_pool_seq.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let driver_name = self.driver_name.file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.to_owned())
            .unwrap_or("BINDER".to_owned());
        format!("{}:{}_{:X}", driver_name, pid, seq)
    }

    pub(crate) fn spawn_pooled_thread(&self, is_main: bool) {
        if self.thread_pool_started.load(Ordering::Relaxed) {
            let name = self.make_binder_thread_name();
            log::info!("Spawning new pooled thread, name={}", name);
            let _ = thread::Builder::new().name(name).spawn(move || {
                thread_state::join_thread_pool(is_main)
            });

            self.kernel_started_threads.fetch_add(1, Ordering::SeqCst);
        }
        // TODO: if startThreadPool is called on another thread after the process
        // starts up, the kernel might think that it already requested those
        // binder threads, and additional won't be started. This is likely to
        // cause deadlocks, and it will also cause getThreadPoolMaxTotalThreadCount
        // to return too high of a value.
    }

    pub fn join_thread_pool() -> Result<()> {
        thread_state::join_thread_pool(true)
    }
}

fn open_driver(driver: &Path, max_threads: u32) -> std::result::Result<File, Box<dyn std::error::Error>> {
    let fd = File::options()
        .read(true)
        .write(true)
        .open(driver)
        .map_err(|e| format!("Opening '{}' failed: {}\n", driver.to_string_lossy(), e))?;

    let mut vers = binder::binder_version { protocol_version: 0 };

    unsafe {
        let raw_fd = fd.as_raw_fd();
        binder::version(raw_fd, &mut vers)
            .map_err(|e| format!("Binder ioctl to obtain version failed: {}", e))?;
        log::info!("Binder driver protocol version: {}", vers.protocol_version);

        if vers.protocol_version != binder::BINDER_CURRENT_PROTOCOL_VERSION as i32 {
            return Err(format!("Binder driver protocol({}) does not match user space protocol({})!",
                vers.protocol_version, binder::BINDER_CURRENT_PROTOCOL_VERSION).into());
        }

        binder::set_max_threads(raw_fd, &max_threads)
            .map_err(|e| format!("Binder ioctl to set max threads failed: {}", e))?;
        log::info!("Binder driver max threads set to {}", max_threads);

        let enable = DEFAULT_ENABLE_ONEWAY_SPAM_DETECTION;
        binder::enable_oneway_spam_detection(raw_fd, &enable)
            .map_err(|e| format!("Binder ioctl to enable oneway spam detection failed: {}", e))?;
    }

    Ok(fd)
}

impl Drop for ProcessState {
    fn drop(self: &mut ProcessState) {
        let mut mmap = self.mmap.write().unwrap();
        unsafe { libc::munmap(mmap.ptr, mmap.size); }
        mmap.ptr = std::ptr::null_mut();
        mmap.size = 0;
    }
}