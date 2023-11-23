// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashMap;
use std::sync::{Arc};
use std::path::Path;
use std::fs::File;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{RwLock};


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

pub struct ProcessState {
    driver: File,
    mmap: RwLock<(*mut std::ffi::c_void, usize)>,
    context_manager: RwLock<Option<Arc<dyn Transactable>>>,
    handle_to_object: RwLock<HashMap<u32, WeakIBinder>>,
    disable_background_scheduling: AtomicBool,
    call_restriction: RwLock<CallRestriction>,
}

unsafe impl Sync for ProcessState {}
unsafe impl Send for ProcessState {}

impl ProcessState {
    fn instance() -> &'static OnceLock<ProcessState> {
        static INSTANCE: OnceLock<ProcessState> = OnceLock::new();
        &INSTANCE
    }

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

    pub fn init(driver_name: &str, max_threads: u32) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let max_threads = if max_threads < DEFAULT_MAX_BINDER_THREADS {
            max_threads
        } else {
            DEFAULT_MAX_BINDER_THREADS
        };

        let driver = open_driver(Path::new(driver_name), max_threads)?;

        let vm_size = ((1024 * 1024) - unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) } * 2) as usize;

        let mmap = unsafe {
            let vm_start = libc::mmap(std::ptr::null_mut(),
                vm_size,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE, driver.as_raw_fd(), 0);

            if vm_start == libc::MAP_FAILED {
                return Err(format!("{} mmap is failed!", driver_name).into());
            }

            (vm_start, vm_size)
        };

        let this = ProcessState {
            driver,
            mmap: RwLock::new(mmap),
            context_manager: RwLock::new(None),
            handle_to_object: RwLock::new(HashMap::new()),
            disable_background_scheduling: AtomicBool::new(false),
            call_restriction: RwLock::new(CallRestriction::None),
        };

        Self::instance().set(this).map_err(|_| "ProcessState::init() is failed due to OnceLock::set() error!")?;

        Ok(())
    }

    pub fn become_context_manager(&self, transactable: Arc<dyn Transactable>) -> std::result::Result<(), Box<dyn std::error::Error>> {
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

        *self.context_manager.write().unwrap() = Some(transactable);

        Ok(())
    }

    pub fn context_manager(&self) -> Option<Arc<dyn Transactable>> {
        self.context_manager.read().unwrap().clone()
    }

    pub fn context_object(&self) -> Result<StrongIBinder> {
        self.strong_proxy_for_handle(0)
        // , Box::new(BpServiceManager::new())
    }

    pub fn strong_proxy_for_handle(&self, handle: u32) -> Result<StrongIBinder> {
        if let Some(weak) = self.handle_to_object.read().unwrap().get(&handle) {
            return Ok(weak.upgrade())
        }

        if handle == 0 {
            let original_call_restriction = thread_state::call_restriction();
            thread_state::set_call_restriction(CallRestriction::None);

            thread_state::ping_binder(handle)?;

            thread_state::set_call_restriction(original_call_restriction);
        }

        let interface = thread_state::query_interface(handle)?;

        let weak = WeakIBinder::new(ProxyHandle::new(handle, interface));

        self.handle_to_object.write().unwrap().insert(handle, weak.clone());

        Ok(weak.upgrade())
    }

    pub fn disable_background_scheduling(& self, disable: bool) {
        self.disable_background_scheduling.store(disable, Ordering::Relaxed);
    }

    pub fn background_scheduling_disabled(&self) -> bool {
        self.disable_background_scheduling.load(Ordering::Relaxed)
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

        if vers.protocol_version != binder::BINDER_CURRENT_PROTOCOL_VERSION as i32 {
            return Err(format!("Binder driver protocol({}) does not match user space protocol({})!",
                vers.protocol_version, binder::BINDER_CURRENT_PROTOCOL_VERSION).into());
        }

        binder::set_max_threads(raw_fd, &max_threads)
            .map_err(|e| format!("Binder ioctl to set max threads failed: {}", e))?;

        let enable = DEFAULT_ENABLE_ONEWAY_SPAM_DETECTION;
        binder::enable_oneway_spam_detection(raw_fd, &enable)
            .map_err(|e| format!("Binder ioctl to enable oneway spam detection failed: {}", e))?;
    }

    Ok(fd)
}

impl AsRawFd for ProcessState {
    fn as_raw_fd(&self) -> RawFd {
        self.driver.as_raw_fd()
    }
}

impl Drop for ProcessState {
    fn drop(self: &mut ProcessState) {
        unsafe {
            let mut mmap = self.mmap.write().unwrap();
            libc::munmap(mmap.0, mmap.1);
            mmap.0 = std::ptr::null_mut();
        }
    }
}