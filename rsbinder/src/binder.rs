// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

/*
 * Copyright (C) 2020 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::ops::Deref;
use std::sync::atomic::*;
use std::hash::{Hash, Hasher};

use std::any::Any;
use std::sync::{Arc};
use std::fs::File;
use std::fmt::Debug;
use crate::{
    error::*,
    parcel::*,
    native,
    proxy,
    thread_state,
};

// use crate::thread_state::*;

/// Binder action to perform.
///
/// This must be a number between [`FIRST_CALL_TRANSACTION`] and
/// [`LAST_CALL_TRANSACTION`].
pub type TransactionCode = u32;

/// Additional operation flags.
///
/// `FLAG_*` values.
pub type TransactionFlags = u32;

const fn b_pack_chars(c1: char, c2: char, c3: char, c4: char) -> u32 {
    ((c1 as u32)<<24) | ((c2 as u32)<<16) | ((c3 as u32)<<8) | (c4 as u32)
}

pub const FIRST_CALL_TRANSACTION: u32 = 0x00000001;
pub const LAST_CALL_TRANSACTION: u32 = 0x00ffffff;

pub const PING_TRANSACTION: u32 = b_pack_chars('_', 'P', 'N', 'G');
pub const DUMP_TRANSACTION: u32 = b_pack_chars('_', 'D', 'M', 'P');
pub const SHELL_COMMAND_TRANSACTION: u32 = b_pack_chars('_', 'C', 'M', 'D');
// It must be used to ask binder's interface name.
pub const INTERFACE_TRANSACTION: u32 = b_pack_chars('_', 'N', 'T', 'F');
pub const SYSPROPS_TRANSACTION: u32 = b_pack_chars('_', 'S', 'P', 'R');
pub const EXTENSION_TRANSACTION: u32 = b_pack_chars('_', 'E', 'X', 'T');
pub const DEBUG_PID_TRANSACTION: u32 = b_pack_chars('_', 'P', 'I', 'D');

        // See android.os.IBinder.TWEET_TRANSACTION
        // Most importantly, messages can be anything not exceeding 130 UTF-8
        // characters, and callees should exclaim "jolly good message old boy!"
pub const TWEET_TRANSACTION: u32 = b_pack_chars('_', 'T', 'W', 'T');

        // See android.os.IBinder.LIKE_TRANSACTION
        // Improve binder self-esteem.
pub const LIKE_TRANSACTION: u32 = b_pack_chars('_', 'L', 'I', 'K');

        // Corresponds to TF_ONE_WAY -- an asynchronous call.
pub const FLAG_ONEWAY: u32 = 0x00000001;

        // Corresponds to TF_CLEAR_BUF -- clear transaction buffers after call
        // is made
pub const FLAG_CLEAR_BUF: u32 = 0x00000020;

        // Private userspace flag for transaction which is being requested from
        // a vendor context.
pub const FLAG_PRIVATE_VENDOR: u32 = 0x10000000;

pub const INTERFACE_HEADER: u32  = b_pack_chars('S', 'Y', 'S', 'T');


/// Super-trait for Binder interfaces.
///
/// This trait allows conversion of a Binder interface trait object into an
/// IBinder object for IPC calls. All Binder remotable interface (i.e. AIDL
/// interfaces) must implement this trait.
///
/// This is equivalent `IInterface` in C++.
pub trait Interface: Send + Sync {
    // fn as_any(&self) -> &dyn Any;

    // /// Convert this binder object into a generic [`SpIBinder`] reference.
    fn as_binder(&self) -> StrongIBinder {
        panic!("This object was not a Binder object and cannot be converted into an StrongIBinder.")
    }

    /// Dump transaction handler for this Binder object.
    ///
    /// This handler is a no-op by default and should be implemented for each
    /// Binder service struct that wishes to respond to dump transactions.
    fn dump(&self, _file: &File, _args: &[&str]) -> Result<()> {
        Ok(())
    }

    // fn box_clone(&self) -> Box<dyn Interface>;
}

// impl Clone for Box<dyn Interface> {
//     fn clone(&self) -> Box<dyn Interface> {
//         self.box_clone()
//     }
// }

// pub(crate) struct Unknown {}

// impl Interface for Unknown {
//     fn box_clone(&self) -> Box<dyn Interface> {
//         Box::new(Self {})
//     }
// }


// ///
// /// # Example
// ///
// /// For Binder interface `IFoo`, the following implementation should be made:
// /// ```no_run
// /// # use binder::{FromIBinder, SpIBinder, Result};
// /// # trait IFoo {}
// /// impl FromIBinder for dyn IFoo {
// ///     fn try_from(ibinder: SpIBinder) -> Result<Box<Self>> {
// ///         // ...
// ///         # Err(binder::StatusCode::OK)
// ///     }
// /// }
// /// ```
// pub trait FromIBinder: Interface {
//     /// Try to interpret a generic Binder object as this interface.
//     ///
//     /// Returns a trait object for the `Self` interface if this object
//     /// implements that interface.
//     fn try_from(ibinder: Arc<Box<dyn IBinder>>) -> Result<Arc<Self>>;
// }


pub trait DeathRecipient {
    fn binder_died(&mut self, who: WeakIBinder);
}

/// Interface of binder local or remote objects.
///
/// This trait corresponds to the parts of the interface of the C++ `IBinder`
/// class which are public.
pub trait IBinder: Send + Sync {
    /// Register the recipient for a notification if this binder
    /// goes away. If this binder object unexpectedly goes away
    /// (typically because its hosting process has been killed),
    /// then the `DeathRecipient`'s callback will be called.
    ///
    /// You will only receive death notifications for remote binders,
    /// as local binders by definition can't die without you dying as well.
    /// Trying to use this function on a local binder will result in an
    /// INVALID_OPERATION code being returned and nothing happening.
    ///
    /// This link always holds a weak reference to its recipient.
    fn link_to_death(&mut self, recipient: &mut dyn DeathRecipient) -> Result<()>;

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    fn unlink_to_death(&mut self, recipient: &mut dyn DeathRecipient) -> Result<()>;

    /// Send a ping transaction to this object
    fn ping_binder(&self) -> Result<()>;

    fn as_any(&self) -> &dyn Any;
    fn is_remote(&self) -> bool;
}

impl dyn IBinder {
    pub fn as_native<T: 'static + Remotable>(&self) -> Option<&native::Binder<T>> {
        self.as_any().downcast_ref::<native::Binder<T>>()
    }

    pub fn as_proxy(&self) -> Option<&proxy::ProxyHandle> {
        self.as_any().downcast_ref::<proxy::ProxyHandle>()
    }
}

impl std::fmt::Debug for dyn IBinder {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self.as_any())
    }
}

impl PartialEq for dyn IBinder  {
    fn eq(&self, other: &Self) -> bool {
        if self.is_remote() && other.is_remote() {
            return self.as_proxy() == other.as_proxy()
        } else if !self.is_remote() && !other.is_remote() {
            return self.as_any().type_id() == other.as_any().type_id()
        }

        false
    }
}


pub fn cookie_for_binder(binder: Arc<dyn IBinder>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    Arc::as_ptr(&binder).hash(&mut hasher);
    hasher.finish()
}

/// A local service that can be remotable via Binder.
///
/// An object that implement this interface made be made into a Binder service
/// via `Binder::new(object)`.
///
/// This is a low-level interface that should normally be automatically
/// generated from AIDL via the [`declare_binder_interface!`] macro. When using
/// the AIDL backend, users need only implement the high-level AIDL-defined
/// interface. The AIDL compiler then generates a container struct that wraps
/// the user-defined service and implements `Remotable`.
pub trait Remotable: Send + Sync {
    /// The Binder interface descriptor string.
    ///
    /// This string is a unique identifier for a Binder interface, and should be
    /// the same between all implementations of that interface.
    fn get_descriptor() -> &'static str where Self: Sized;

    /// Handle and reply to a request to invoke a transaction on this object.
    ///
    /// `reply` may be [`None`] if the sender does not expect a reply.
    fn on_transact(&self, code: TransactionCode, reader: &mut Parcel, reply: &mut Parcel) -> Result<()>;

    /// Handle a request to invoke the dump transaction on this
    /// object.
    fn on_dump(&self, file: &File, args: &[&str]) -> Result<()>;
}

pub trait Transactable {
    fn transact(&self, code: TransactionCode, reader: &mut Parcel, reply: &mut Parcel) -> Result<()>;
}


// pub trait InterfaceClassMethods {
//     /// Get the interface descriptor string for this object type.
//     fn get_descriptor() -> &'static str
//     where
//         Self: Sized;
//
//     /// Called during construction of a new `AIBinder` object of this interface
//     /// class.
//     ///
//     /// The opaque pointer parameter will be the parameter provided to
//     /// `AIBinder_new`. Returns an opaque userdata to be associated with the new
//     /// `AIBinder` object.
//     ///
//     /// # Safety
//     ///
//     /// Callback called from C++. The parameter argument provided to
//     /// `AIBinder_new` must match the type expected here. The `AIBinder` object
//     /// will take ownership of the returned pointer, which it will free via
//     /// `on_destroy`.
//     fn on_create();
//
//     // /// Called when a transaction needs to be processed by the local service
//     // /// implementation.
//     // ///
//     // /// # Safety
//     // ///
//     // /// Callback called from C++. The `binder` parameter must be a valid pointer
//     // /// to a binder object of this class with userdata initialized via this
//     // /// class's `on_create`. The parcel parameters must be valid pointers to
//     // /// parcel objects.
//     // fn on_transact<T: Remotable>(
//     //     binder: &mut native::Binder<T>,
//     //     code: u32,
//     //     data: &parcel::Reader,
//     //     reply: &parcel::Writer,
//     // ) -> Result<()>;
//
//     /// Called whenever an `AIBinder` object is no longer referenced and needs
//     /// to be destroyed.
//     ///
//     /// # Safety
//     ///
//     /// Callback called from C++. The opaque pointer parameter must be the value
//     /// returned by `on_create` for this class. This function takes ownership of
//     /// the provided pointer and destroys it.
//     fn on_destroy();
//
//     /// Called to handle the `dump` transaction.
//     ///
//     /// # Safety
//     ///
//     /// Must be called with a non-null, valid pointer to a local `AIBinder` that
//     /// contains a `T` pointer in its user data. fd should be a non-owned file
//     /// descriptor, and args must be an array of null-terminated string
//     /// poiinters with length num_args.
//     fn on_dump<T: Remotable>(binder: &mut native::Binder<T>, fd: i32, args: &str, num_args: u32) -> Result<()>;
// }


/// Opaque reference to the type of a Binder interface.
///
/// This object encapsulates the Binder interface descriptor string, along with
/// the binder transaction callback, if the class describes a local service.
///
/// A Binder remotable object may only have a single interface class, and any
/// given object can only be associated with one class. Two objects with
/// different classes are incompatible, even if both classes have the same
/// interface descriptor.
// #[derive(Clone, PartialEq, Eq)]
// pub struct InterfaceClass<I: InterfaceClassMethods> {
//     descriptor: String,
//     class_methods: I,
// }
//
// impl<I: InterfaceClassMethods> InterfaceClass<I> {
//     /// Get a Binder NDK `AIBinder_Class` pointer for this object type.
//     ///
//     /// Note: the returned pointer will not be constant. Calling this method
//     /// multiple times for the same type will result in distinct class
//     /// pointers. A static getter for this value is implemented in
//     /// [`declare_binder_interface!`].
//     pub fn new(methods: I) -> InterfaceClass<I> {
//         InterfaceClass {
//             descriptor: I::get_descriptor().to_string(),
//             class_methods: methods,
//         }
//     }
//
//     // /// Construct an `InterfaceClass` out of a raw, non-null `AIBinder_Class`
//     // /// pointer.
//     // ///
//     // /// # Safety
//     // ///
//     // /// This function is safe iff `ptr` is a valid, non-null pointer to an
//     // /// `AIBinder_Class`.
//     // pub(crate) unsafe fn from_ptr(ptr: *const sys::AIBinder_Class) -> InterfaceClass {
//     //     InterfaceClass(ptr)
//     // }
//
//     /// Get the interface descriptor string of this class.
//     pub fn get_descriptor(&self) -> String {
//         self.descriptor.clone()
//     }
// }


/// Interface stability promise
///
/// An interface can promise to be a stable vendor interface ([`Vintf`]), or
/// makes no stability guarantees ([`Local`]). [`Local`] is
/// currently the default stability.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[derive(Default)]
pub enum Stability {
    /// Default stability, visible to other modules in the same compilation
    /// context (e.g. modules on system.img)
    #[default]
    Local,

    /// A Vendor Interface Object, which promises to be stable
    Vintf,
}

impl From<Stability> for i32 {
    fn from(stability: Stability) -> i32 {
        use Stability::*;
        match stability {
            Local => 0,
            Vintf => 1,
        }
    }
}

impl TryFrom<i32> for Stability {
    type Error = StatusCode;
    fn try_from(stability: i32) -> Result<Stability> {
        use Stability::*;
        match stability {
            0 => Ok(Local),
            1 => Ok(Vintf),
            _ => Err(StatusCode::BadValue)
        }
    }
}

const INITIAL_STRONG_VALUE: usize = i32::MAX as _;

#[derive(Debug)]
struct Inner {
    strong: AtomicUsize,
    data: Box<dyn IBinder>,
}

impl Inner {
    fn new(data: Box<dyn IBinder>) -> Arc<Self> {
        Arc::new(
            Self {
                strong: AtomicUsize::new(INITIAL_STRONG_VALUE),
                data,
            }
        )
    }
}

impl PartialEq for Inner {
    fn eq(&self, other: &Self) -> bool {
        self.data.eq(&other.data)
    }
}

impl Drop for Inner {
    fn drop(self: &mut Inner) {
        if let Some(proxy) = self.data.as_proxy() {
            thread_state::dec_weak_handle(proxy.handle())
                .expect("Failed to decrease the binder weak reference count.")
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct StrongIBinder {
    inner: Arc<Inner>,
}

impl StrongIBinder {
    pub fn new(data: Box<dyn IBinder>) -> Self {
        let this = WeakIBinder::new(data).upgrade();
        this.inc_strong();
        this
    }

    fn new_with_inner(inner: Arc<Inner>) -> Self {
        let this = Self { inner };
        this.inc_strong();
        this
    }

    pub fn downgrade(this: &Self) -> WeakIBinder {
        WeakIBinder::new_with_inner(this.inner.clone())
        // drop will be called.
    }

    fn inc_strong(&self) {
        let c = self.inner.strong.fetch_add(1, Ordering::Relaxed);
        if c == INITIAL_STRONG_VALUE {
            self.inner.strong.fetch_sub(INITIAL_STRONG_VALUE, Ordering::Relaxed);
            if let Some(proxy) = self.inner.data.as_proxy() {
                thread_state::inc_strong_handle(proxy.handle(), self.clone())
                    .expect("Failed to increase the binder strong reference count.");
            }
        }
    }

    fn dec_strong(&self) {
        let c = self.inner.strong.fetch_sub(1, Ordering::Relaxed);
        if c == 1 {
            if let Some(proxy) = self.inner.data.as_proxy() {
                thread_state::dec_strong_handle(proxy.handle())
                    .expect("Failed to decrease the binder strong reference count.");
            }
        }
    }
}

impl Clone for StrongIBinder {
    fn clone(&self) -> Self {
        Self::new_with_inner(self.inner.clone())
    }
}

impl Drop for StrongIBinder {
    fn drop(&mut self) {
        self.dec_strong();
    }
}

impl Deref for StrongIBinder {
    type Target = Box<dyn IBinder>;
    fn deref(&self) -> &Self::Target {
        &self.inner.data
    }
}

#[derive(Clone)]
pub struct WeakIBinder {
    inner: Arc<Inner>,
}

impl WeakIBinder {
    pub(crate) fn new(data: Box<dyn IBinder>) -> Self {
        let this = Self { inner: Inner::new(data) };

        if let Some(proxy) = this.inner.data.as_proxy() {
            thread_state::inc_weak_handle(proxy.handle(), this.clone())
                .expect("Failed to increase the binder weak reference count.")
        }

        this
    }

    fn new_with_inner(inner: Arc<Inner>) -> Self {
        Self { inner }
    }

    pub fn upgrade(&self) -> StrongIBinder {
        StrongIBinder::new_with_inner(self.inner.clone())
    }
}

impl Deref for WeakIBinder {
    type Target = Box<dyn IBinder>;
    fn deref(&self) -> &Self::Target {
        &self.inner.data
    }
}

#[cfg(test)]
mod tests {
    use crate::proxy::ProxyHandle;
    use super::*;

    #[test]
    fn test_strong() -> Result<()> {
        let strong = StrongIBinder::new(ProxyHandle::new(0, "interface".to_owned()));
        assert_eq!(strong.inner.strong.load(Ordering::Relaxed), 1);

        let strong2 = strong.clone();
        assert_eq!(strong2.inner.strong.load(Ordering::Relaxed), 2);


        let weak = StrongIBinder::downgrade(&strong);

        assert_eq!(weak.inner.strong.load(Ordering::Relaxed), 1);

        let strong = weak.upgrade();
        assert_eq!(strong.inner.strong.load(Ordering::Relaxed), 2);
        StrongIBinder::downgrade(&strong);
        // assert_eq!(*strong2.0.lock().unwrap(), 101);

        // let weak = strong2.downgrade();

        // assert_eq!(*weak.0.lock().unwrap(), 1);

        Ok(())
    }
}
