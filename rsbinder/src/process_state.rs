// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashMap;
use std::sync::{Arc};
use std::path::Path;
use std::fs::File;
use std::os::unix::io::{AsRawFd, RawFd, IntoRawFd};
use std::sync::{RwLock};


use crate::{
    error::*,
    binder::*,
    sys::binder,
    proxy::*,
    native,
    thread_state,
    // service_manager::{BnServiceManager},
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

lazy_static! {
    static ref PROCESS_STATE: Arc<ProcessState> = Arc::new(ProcessState::new());
}

pub struct ProcessState {
    driver_fd: RwLock<RawFd>,
    mmap: RwLock<(*mut std::ffi::c_void, usize)>,
    context_manager: RwLock<Option<Arc<dyn Transactable>>>,
    handle_to_object: RwLock<HashMap<u32, WeakIBinder>>,
    disable_background_scheduling: AtomicBool,
    call_restriction: RwLock<CallRestriction>,
}

unsafe impl Sync for ProcessState {}
unsafe impl Send for ProcessState {}

impl ProcessState {
    fn new() -> Self {
        ProcessState {
            driver_fd: RwLock::new(-1),
            mmap: RwLock::new((std::ptr::null_mut(), 0)),
            context_manager: RwLock::new(None),
            handle_to_object: RwLock::new(HashMap::new()),
            disable_background_scheduling: AtomicBool::new(false),
            call_restriction: RwLock::new(CallRestriction::None),
        }
    }

    pub fn as_self() -> Arc<ProcessState> {
        PROCESS_STATE.clone()
    }

    pub fn set_call_restriction(&self, call_restriction: CallRestriction) {
        let mut self_call_restriction = self.call_restriction.write().unwrap();
        *self_call_restriction = call_restriction;
    }

    pub(crate) fn call_restriction(&self) -> CallRestriction {
        *self.call_restriction.read().unwrap()
    }

    pub fn init(& self, driver: &str, max_threads: u32) -> bool {
        if *self.driver_fd.read().unwrap() != -1 {
            log::warn!("ProcessState has been initialized.");
            return false;
        }

        let max_threads = if max_threads < DEFAULT_MAX_BINDER_THREADS {
            max_threads
        } else {
            DEFAULT_MAX_BINDER_THREADS
        };

        *self.driver_fd.write().unwrap() = match open_driver(Path::new(driver), max_threads) {
            Some(fd) => fd,
            None => return false
        };

        let vm_size = ((1024 * 1024) - unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) } * 2) as usize;

        unsafe {
            let mut driver_fd = self.driver_fd.write().unwrap();
            let vm_start = libc::mmap(std::ptr::null_mut(),
                vm_size,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE, *driver_fd, 0);

            if vm_start == libc::MAP_FAILED {
                libc::close(*driver_fd);
                *driver_fd = -1;
                return false;
            }

            *self.mmap.write().unwrap() = (vm_start, vm_size);
        }

        true
    }

    pub fn become_context_manager(&self, transactable: Arc<dyn Transactable>) -> bool {
        let obj = std::mem::MaybeUninit::<binder::flat_binder_object>::zeroed();
        let mut obj = unsafe { obj.assume_init() };
        obj.flags = binder::FLAT_BINDER_FLAG_ACCEPTS_FDS;

        unsafe {
            let driver_fd = self.driver_fd.read().unwrap();
            if binder::set_context_mgr_ext(*driver_fd, &obj).is_err() {
                //     android_errorWriteLog(0x534e4554, "121035042");
                let unused: i32 = 0;
                if let Err(e) = binder::set_context_mgr(*driver_fd, &unused) {
                    log::error!("Binder ioctl to become context manager failed: {}", e.to_string());
                    return false;
                }
            }
        }

        *self.context_manager.write().unwrap() = Some(transactable);

        true
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

fn open_driver(driver: &Path, max_threads: u32) -> Option<RawFd> {
    let fd = File::options()
        .read(true)
        .write(true)
        .open(driver)
        .map_err(|e| log::error!("Opening '{}' failed: {}\n", driver.to_string_lossy(), e.to_string()))
        .ok()?;

    let mut vers = binder::binder_version { protocol_version: 0 };

    unsafe {
        let raw_fd = fd.as_raw_fd();
        binder::version(raw_fd, &mut vers)
            .map_err(|e| log::error!("Binder ioctl to obtain version failed: {}", e.to_string()))
            .ok()?;

        if vers.protocol_version != binder::BINDER_CURRENT_PROTOCOL_VERSION as i32 {
            log::error!("Binder driver protocol({}) does not match user space protocol({})!",
                vers.protocol_version, binder::BINDER_CURRENT_PROTOCOL_VERSION);
            return None;
        }

        binder::set_max_threads(raw_fd, &max_threads)
            .map_err(|e| log::error!("Binder ioctl to set max threads failed: {}", e.to_string()))
            .ok()?;

        let enable = DEFAULT_ENABLE_ONEWAY_SPAM_DETECTION;
        binder::enable_oneway_spam_detection(raw_fd, &enable)
            .map_err(|e| log::error!("Binder ioctl to enable oneway spam detection failed: {}", e.to_string()))
            .ok()?;
    }

    Some(fd.into_raw_fd())
}

impl AsRawFd for ProcessState {
    fn as_raw_fd(&self) -> RawFd {
        *self.driver_fd.read().unwrap()
    }
}

impl Drop for ProcessState {
    fn drop(self: &mut ProcessState) {
        let mut driver_fd = self.driver_fd.write().unwrap();
        if *driver_fd != -1 {
            unsafe {
                let mut mmap = self.mmap.write().unwrap();
                libc::munmap(mmap.0, mmap.1);
                libc::close(*driver_fd);
                mmap.0 = std::ptr::null_mut();
            }
            *driver_fd = -1;
        }
    }
}