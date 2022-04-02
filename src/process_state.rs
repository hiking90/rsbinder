use std::sync::Arc;
use std::path::Path;
use std::fs::File;
use std::os::unix::io::{AsRawFd, RawFd, IntoRawFd};
use std::sync::{RwLock, Once};
use log;

use crate::sys::binder;
use crate::binder::*;
use crate::{native, service_manager::BnServiceManager};

const DEFAULT_MAX_BINDER_THREADS: u32 = 15;
const DEFAULT_ENABLE_ONEWAY_SPAM_DETECTION: u32 = 1;

static mut PROCESS_STATE: std::mem::MaybeUninit<Arc<RwLock<ProcessState>>> = std::mem::MaybeUninit::uninit();
static ONCE: Once = Once::new();

pub struct ProcessState {
    driver: RawFd,
    vm_start: *mut libc::c_void,
    vm_size: usize,
    context_manager: Option<native::Binder<BnServiceManager>>,
}

impl ProcessState {
    fn new() -> Self {
        ProcessState {
            driver: -1,
            vm_start: std::ptr::null_mut(),
            vm_size: 0,
            context_manager: None,
        }
    }

    pub fn as_self() -> &'static Arc<RwLock<ProcessState>> {
        unsafe {
            ONCE.call_once(|| {
                let process = Arc::new(RwLock::new(ProcessState::new()));
                PROCESS_STATE.write(process);
            });
            PROCESS_STATE.assume_init_ref()
        }
    }

    pub fn init(&mut self, driver: &str, max_threads: u32) -> bool {
        if self.driver != -1 {
            log::warn!("ProcessState has been initialized.");
            return false;
        }

        let max_threads = if max_threads < DEFAULT_MAX_BINDER_THREADS {
            max_threads
        } else {
            DEFAULT_MAX_BINDER_THREADS
        };

        self.driver = match open_driver(Path::new(driver), max_threads) {
            Some(fd) => fd,
            None => return false
        };

        self.vm_size = ((1 * 1024 * 1024) - unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) } * 2) as usize;

        unsafe {
            self.vm_start = libc::mmap(std::ptr::null_mut(),
                self.vm_size,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE, self.driver, 0);

            if self.vm_start == libc::MAP_FAILED {
                libc::close(self.driver);
                self.driver = -1;
                return false;
            }
        }

        true
    }

    pub fn become_context_manager(&mut self) -> bool {
        let obj = std::mem::MaybeUninit::<binder::flat_binder_object>::zeroed();
        let mut obj = unsafe { obj.assume_init() };
        obj.flags = binder::FLAT_BINDER_FLAG_ACCEPTS_FDS;

        unsafe {
            if let Err(_) = binder::set_context_mgr_ext(self.driver, &obj) {
                //     android_errorWriteLog(0x534e4554, "121035042");
                let unused: i32 = 0;
                if let Err(e) = binder::set_context_mgr(self.driver, &unused) {
                    log::error!("Binder ioctl to become context manager failed: {}", e.to_string());
                    return false;
                }
            }
        }

        self.context_manager = Some(native::Binder::new(BnServiceManager::new()));

        true
    }

    pub fn context_manager(&self) -> Option<&native::Binder<BnServiceManager>> {
        self.context_manager.as_ref()
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

        let max_threads = max_threads;
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
        self.driver
    }
}

impl Drop for ProcessState {
    fn drop(&mut self) {
        if self.driver != -1 {
            unsafe {
                libc::munmap(self.vm_start, self.vm_size);
                libc::close(self.driver);
            }
            self.driver = -1;
        }
    }
}