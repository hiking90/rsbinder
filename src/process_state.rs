use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::path::Path;
use std::fs::File;
use std::os::unix::io::{AsRawFd, RawFd, IntoRawFd};
use std::sync::{RwLock};
use log;

use crate::{
    error::*,
    binder::*,
    sys::binder,
    proxy::*,
    parcel::*,
    native,
    thread_state,
    service_manager::{BnServiceManager, BpServiceManager},
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

struct Inner {
    driver_fd: RawFd,
    vm_start: *mut std::ffi::c_void,
    vm_size: usize,
    context_manager: Option<Arc<native::Binder<BnServiceManager>>>,
    handle_to_object: HashMap<u32, Weak<dyn IBinder>>,
    disable_background_scheduling: bool,
    call_restriction: CallRestriction,
}

pub struct ProcessState {
    inner: RwLock<Inner>,
}

unsafe impl Sync for ProcessState {}
unsafe impl Send for ProcessState {}

impl ProcessState {
    fn new() -> Self {
        ProcessState {
            inner: RwLock::new(Inner {
                driver_fd: -1,
                vm_start: std::ptr::null_mut(),
                vm_size: 0,
                context_manager: None,
                handle_to_object: HashMap::new(),
                disable_background_scheduling: false,
                call_restriction: CallRestriction::None,
            }),
        }
    }

    pub fn as_self() -> Arc<ProcessState> {
        PROCESS_STATE.clone()
    }

    pub fn set_call_restriction(&self, call_restriction: CallRestriction) {
        let mut inner = self.inner.write().unwrap();
        inner.call_restriction = call_restriction;
    }

    pub(crate) fn call_restriction(&self) -> CallRestriction {
        let inner = self.inner.read().unwrap();
        inner.call_restriction
    }

    pub fn init(& self, driver: &str, max_threads: u32) -> bool {
        let mut inner = self.inner.write().unwrap();
        if inner.driver_fd != -1 {
            log::warn!("ProcessState has been initialized.");
            return false;
        }

        let max_threads = if max_threads < DEFAULT_MAX_BINDER_THREADS {
            max_threads
        } else {
            DEFAULT_MAX_BINDER_THREADS
        };

        inner.driver_fd = match open_driver(Path::new(driver), max_threads) {
            Some(fd) => fd,
            None => return false
        };

        inner.vm_size = ((1 * 1024 * 1024) - unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) } * 2) as usize;

        unsafe {
            let vm_start = libc::mmap(std::ptr::null_mut(),
                inner.vm_size,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE, inner.driver_fd, 0);

            if vm_start == libc::MAP_FAILED {
                libc::close(inner.driver_fd);
                inner.driver_fd = -1;
                return false;
            }

            inner.vm_start = vm_start;
        }

        true
    }

    pub fn become_context_manager(& self) -> bool {
        let mut inner = self.inner.write().unwrap();

        let obj = std::mem::MaybeUninit::<binder::flat_binder_object>::zeroed();
        let mut obj = unsafe { obj.assume_init() };
        obj.flags = binder::FLAT_BINDER_FLAG_ACCEPTS_FDS;

        unsafe {
            if let Err(_) = binder::set_context_mgr_ext(inner.driver_fd, &obj) {
                //     android_errorWriteLog(0x534e4554, "121035042");
                let unused: i32 = 0;
                if let Err(e) = binder::set_context_mgr(inner.driver_fd, &unused) {
                    log::error!("Binder ioctl to become context manager failed: {}", e.to_string());
                    return false;
                }
            }
        }

        inner.context_manager = Some(Arc::new(native::Binder::new(BnServiceManager::new())));

        true
    }

    pub fn context_manager(&self) -> Option<Arc<native::Binder<BnServiceManager>>> {
        let inner = self.inner.read().unwrap();
        inner.context_manager.clone()
    }

    pub fn context_object(&self) -> Option<Arc<Proxy<BpServiceManager>>>{
        todo!()
        // sp<IBinder> context = getStrongProxyForHandle(0);

        // if (context) {
        //     // The root object is special since we get it directly from the driver, it is never
        //     // written by Parcell::writeStrongBinder.
        //     internal::Stability::markCompilationUnit(context.get());
        // } else {
        //     ALOGW("Not able to get context object on %s.", mDriverName.c_str());
        // }

        // return context;
    }

    pub fn strong_proxy_for_handle(&self, handle: u32) -> Result<Arc<dyn IBinder>> {
        let mut inner = self.inner.write().unwrap();

        match inner.handle_to_object.get(&handle) {
            Some(binder) => {
                return binder.upgrade().ok_or(ErrorKind::DeadObject.into());
            },
            None => {
                if handle == 0 {
                    let original_call_restriction = thread_state::call_restriction();
                    thread_state::set_call_restriction(CallRestriction::None);

                    let data = Parcel::new();
                    thread_state::transact(0, PING_TRANSACTION, &data, None, 0)?;

                    thread_state::set_call_restriction(original_call_restriction);

        //             // Special case for context manager...
        //             // The context manager is the only object for which we create
        //             // a BpBinder proxy without already holding a reference.
        //             // Perform a dummy transaction to ensure the context manager
        //             // is registered before we create the first local reference
        //             // to it (which will occur when creating the BpBinder).
        //             // If a local reference is created for the BpBinder when the
        //             // context manager is not present, the driver will fail to
        //             // provide a reference to the context manager, but the
        //             // driver API does not return status.
        //             //
        //             // Note that this is not race-free if the context manager
        //             // dies while this code runs.

        //             IPCThreadState* ipc = IPCThreadState::self();

        //             CallRestriction originalCallRestriction = ipc->getCallRestriction();
        //             ipc->setCallRestriction(CallRestriction::NONE);

        //             Parcel data;
        //             status_t status = ipc->transact(
        //                     0, IBinder::PING_TRANSACTION, data, nullptr, 0);

        //             ipc->setCallRestriction(originalCallRestriction);

        //             if (status == DEAD_OBJECT)
        //                return nullptr;
                }
            }
        }

        let proxy: Arc<dyn IBinder> = Arc::new(Proxy::new(handle, Unknown {}));
        // if handle != 0 {
        //     thread_state::inc_strong_handle(handle, proxy.clone())?;
        // }
        inner.handle_to_object.insert(handle, Arc::downgrade(&proxy));

        Ok(proxy)


        // sp<IBinder> result;

        // AutoMutex _l(mLock);

        // handle_entry* e = lookupHandleLocked(handle);

        // if (e != nullptr) {
        //     // We need to create a new BpBinder if there isn't currently one, OR we
        //     // are unable to acquire a weak reference on this current one.  The
        //     // attemptIncWeak() is safe because we know the BpBinder destructor will always
        //     // call expungeHandle(), which acquires the same lock we are holding now.
        //     // We need to do this because there is a race condition between someone
        //     // releasing a reference on this BpBinder, and a new reference on its handle
        //     // arriving from the driver.
        //     IBinder* b = e->binder;
        //     if (b == nullptr || !e->refs->attemptIncWeak(this)) {
        //         if (handle == 0) {
        //             // Special case for context manager...
        //             // The context manager is the only object for which we create
        //             // a BpBinder proxy without already holding a reference.
        //             // Perform a dummy transaction to ensure the context manager
        //             // is registered before we create the first local reference
        //             // to it (which will occur when creating the BpBinder).
        //             // If a local reference is created for the BpBinder when the
        //             // context manager is not present, the driver will fail to
        //             // provide a reference to the context manager, but the
        //             // driver API does not return status.
        //             //
        //             // Note that this is not race-free if the context manager
        //             // dies while this code runs.

        //             IPCThreadState* ipc = IPCThreadState::self();

        //             CallRestriction originalCallRestriction = ipc->getCallRestriction();
        //             ipc->setCallRestriction(CallRestriction::NONE);

        //             Parcel data;
        //             status_t status = ipc->transact(
        //                     0, IBinder::PING_TRANSACTION, data, nullptr, 0);

        //             ipc->setCallRestriction(originalCallRestriction);

        //             if (status == DEAD_OBJECT)
        //                return nullptr;
        //         }

        //         sp<BpBinder> b = BpBinder::PrivateAccessor::create(handle);
        //         e->binder = b.get();
        //         if (b) e->refs = b->getWeakRefs();
        //         result = b;
        //     } else {
        //         // This little bit of nastyness is to allow us to add a primary
        //         // reference to the remote proxy when this team doesn't have one
        //         // but another team is sending the handle to us.
        //         result.force_set(b);
        //         e->refs->decWeak(this);
        //     }
        // }

        // return result;
    }

    pub fn disable_background_scheduling(& self, disable: bool) {
        let mut inner = self.inner.write().unwrap();
        inner.disable_background_scheduling = disable;
    }

    pub fn background_scheduling_disabled(&self) -> bool {
        let inner = self.inner.read().unwrap();
        inner.disable_background_scheduling
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
        let inner = self.inner.read().unwrap();
        inner.driver_fd
    }
}

impl Drop for ProcessState {
    fn drop(self: &mut ProcessState) {
        let mut inner = self.inner.write().unwrap();
        if inner.driver_fd != -1 {
            unsafe {
                libc::munmap(inner.vm_start, inner.vm_size);
                libc::close(inner.driver_fd);
                inner.vm_start = std::ptr::null_mut();
            }
            inner.driver_fd = -1;
        }
    }
}