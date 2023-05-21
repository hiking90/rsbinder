use std::sync::Arc;
use std::any::Any;

use crate::{
    parcel::*,
    binder::*,
    error::*,
    thread_state,
};

pub struct Proxy {
    handle: u32,
    interface: Box<dyn Interface>,
    is_strong: bool,
}

impl Proxy {
    pub fn new(handle: u32, interface: Box<dyn Interface>) -> Result<Arc<Self>> {
        let weak = Arc::new(Self {
            handle: handle,
            interface: interface,
            is_strong: false,
        });

        thread_state::inc_weak_handle(weak.clone())?;

        Ok(weak)
    }

    pub fn handle(&self) -> u32 {
        self.handle
    }

    pub fn interface(&self) -> &Box<dyn Interface> {
        &self.interface
    }

    pub fn is_strong(&self) -> bool {
        self.is_strong
    }

    pub fn upgrade(&self) -> Result<Arc<Self>> {
        let strong = Arc::new(Self {
            handle: self.handle,
            interface: self.interface.clone(),
            is_strong: true,
        });

        thread_state::inc_strong_handle(strong.clone())?;

        Ok(strong)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if self.is_strong {
            if let Err(err) = thread_state::dec_strong_handle(self.handle) {
                log::warn!("Proxy dec_strong_handle error: {:?}", err);
            }
        } else {
            if let Err(err) = thread_state::dec_weak_handle(self.handle) {
                log::warn!("Proxy dec_weak_handle error: {:?}", err);
            }
        }
    }
}

impl IBinder for Proxy {
    fn link_to_death(&mut self, _recipient: &mut dyn DeathRecipient) -> Result<()> {
        todo!("IBinder for Proxy<I> - link_to_death")
    }

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    fn unlink_to_death(&mut self, _recipient: &mut dyn DeathRecipient) -> Result<()> {
        todo!("IBinder for Proxy<I> - unlink_to_death")
    }

    /// Send a ping transaction to this object
    fn ping_binder(&self) -> Result<()> {
        let data = Parcel::new();
        let _reply = thread_state::transact(self.handle, PING_TRANSACTION, &data, 0)?;

        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_remote(&self) -> bool {
        true
    }
}

// /// # Safety
// ///
// /// An `RawIBinder` is an immutable handle to a C++ IBinder, which is thread-safe
// unsafe impl Send for RawIBinder {}

// /// # Safety
// ///
// /// An `RawIBinder` is an immutable handle to a C++ IBinder, which is thread-safe
// unsafe impl Sync for RawIBinder {}

// impl RawIBinder {
//     /// Create an `RawIBinder` wrapper object from a raw `AIBinder` pointer.
//     ///
//     /// # Safety
//     ///
//     /// This constructor is safe iff `ptr` is a null pointer or a valid pointer
//     /// to an `AIBinder`.
//     ///
//     /// In the non-null case, this method conceptually takes ownership of a strong
//     /// reference to the object, so `AIBinder_incStrong` must have been called
//     /// on the pointer before passing it to this constructor. This is generally
//     /// done by Binder NDK methods that return an `AIBinder`, but care should be
//     /// taken to ensure this invariant.
//     ///
//     /// All `RawIBinder` objects that are constructed will hold a valid pointer
//     /// to an `AIBinder`, which will remain valid for the entire lifetime of the
//     /// `RawIBinder` (we keep a strong reference, and only decrement on drop).
//     pub(crate) unsafe fn from_raw(ptr: *mut sys::AIBinder) -> Option<Self> {
//         ptr::NonNull::new(ptr).map(Self)
//     }

//     /// Extract a raw `AIBinder` pointer from this wrapper.
//     ///
//     /// This method should _only_ be used for testing. Do not try to use the NDK
//     /// interface directly for anything else.
//     ///
//     /// # Safety
//     ///
//     /// The resulting pointer is valid only as long as the RawIBinder is alive.
//     /// The RawIBinder object retains ownership of the AIBinder and the caller
//     /// should not attempt to free the returned pointer.
//     pub unsafe fn as_raw(&self) -> *mut sys::AIBinder {
//         self.0.as_ptr()
//     }

//     /// Return true if this binder object is hosted in a different process than
//     /// the current one.
//     pub fn is_remote(&self) -> bool {
//         unsafe {
//             // Safety: `RawIBinder` guarantees that it always contains a valid
//             // `AIBinder` pointer.
//             sys::AIBinder_isRemote(self.as_native())
//         }
//     }

//     /// Try to convert this Binder object into a trait object for the given
//     /// Binder interface.
//     ///
//     /// If this object does not implement the expected interface, the error
//     /// `StatusCode::BAD_TYPE` is returned.
//     pub fn into_interface<I: FromIBinder + Interface + ?Sized>(self) -> Result<Strong<I>> {
//         FromIBinder::try_from(self)
//     }

//     /// Return the interface class of this binder object, if associated with
//     /// one.
//     pub fn get_class(&mut self) -> Option<InterfaceClass> {
//         unsafe {
//             // Safety: `RawIBinder` guarantees that it always contains a valid
//             // `AIBinder` pointer. `AIBinder_getClass` returns either a null
//             // pointer or a valid pointer to an `AIBinder_Class`. After mapping
//             // null to None, we can safely construct an `InterfaceClass` if the
//             // pointer was non-null.
//             let class = sys::AIBinder_getClass(self.as_native_mut());
//             class.as_ref().map(|p| InterfaceClass::from_ptr(p))
//         }
//     }

//     /// Creates a new weak reference to this binder object.
//     pub fn downgrade(&mut self) -> WpIBinder {
//         WpIBinder::new(self)
//     }
// }
