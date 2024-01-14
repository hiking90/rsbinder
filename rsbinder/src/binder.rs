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
use std::sync::{Arc, atomic::*};
use std::any::Any;
use std::fs::File;
use std::fmt::{Debug, Formatter};
use std::marker::PhantomData;
use std::borrow::Borrow;

use crate::{
    error::*,
    parcel::*,
    native,
    proxy,
    thread_state,
};

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
pub const SET_RPC_CLIENT_TRANSACTION: u32 = b_pack_chars('_', 'R', 'P', 'C');

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
    /// Convert this binder object into a generic [`SpIBinder`] reference.
    fn as_binder(&self) -> SIBinder {
        panic!("This object was not a Binder object and cannot be converted into an StrongIBinder.")
    }

    /// Dump transaction handler for this Binder object.
    ///
    /// This handler is a no-op by default and should be implemented for each
    /// Binder service struct that wishes to respond to dump transactions.
    fn dump(&self, _file: &File, _args: &[&str]) -> Result<()> {
        Ok(())
    }
}

///
/// # Example
///
/// For Binder interface `IFoo`, the following implementation should be made:
/// ```no_run
/// # use rsbinder::{FromIBinder, StrongIBinder, Result};
/// # trait IFoo {}
/// impl FromIBinder for dyn IFoo {
///     fn try_from(ibinder: StrongIBinder) -> Result<Box<Self>> {
///         // ...
///         # Err(rsbinder::StatusCode::OK)
///     }
/// }
/// ```
pub trait FromIBinder: Interface {
    /// Try to interpret a generic Binder object as this interface.
    ///
    /// Returns a trait object for the `Self` interface if this object
    /// implements that interface.
    fn try_from(ibinder: SIBinder) -> Result<Strong<Self>>;
}


pub trait DeathRecipient: Send + Sync {
    fn binder_died(&self, who: &WIBinder);
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
    fn link_to_death(&self, recipient: Arc<dyn DeathRecipient>) -> Result<()>;

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    fn unlink_to_death(&self, recipient: Arc<dyn DeathRecipient>) -> Result<()>;

    /// Send a ping transaction to this object
    fn ping_binder(&self) -> Result<()>;

    /// Retrieve the identifier for this object's Binder
    fn id(&self) -> u64;

    /// To support dynamic interface cast, we need to know the interface
    fn as_any(&self) -> &dyn Any;

    /// To convert the interface to a transactable object
    fn as_transactable(&self) -> Option<&dyn Transactable>;
}

impl dyn IBinder {
    /// Convert this binder object into a native binder object.
    pub fn as_native<T: 'static + Remotable>(&self) -> Option<&native::Binder<T>> {
        self.as_any().downcast_ref::<native::Binder<T>>()
    }

    /// Convert this binder object into a proxy binder object.
    pub fn as_proxy(&self) -> Option<&proxy::ProxyHandle> {
        self.as_any().downcast_ref::<proxy::ProxyHandle>()
    }
}

impl std::fmt::Debug for dyn IBinder {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self.as_any())
    }
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
    fn descriptor() -> &'static str where Self: Sized;

    /// Handle and reply to a request to invoke a transaction on this object.
    ///
    /// `reply` may be [`None`] if the sender does not expect a reply.
    fn on_transact(&self, code: TransactionCode, reader: &mut Parcel, reply: &mut Parcel) -> Result<()>;

    /// Handle a request to invoke the dump transaction on this
    /// object.
    fn on_dump(&self, file: &File, args: &[&str]) -> Result<()>;
}

/// A transactable object that can be used to process Binder commands.
pub trait Transactable: Send + Sync {
    fn transact(&self, code: TransactionCode, reader: &mut Parcel, reply: &mut Parcel) -> Result<()>;
}

/// Interface stability promise
///
/// An interface can promise to be a stable vendor interface ([`Vintf`]), or
/// makes no stability guarantees ([`Local`]). [`Local`] is
/// currently the default stability.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stability {
    /// Default stability, visible to other modules in the same compilation
    /// context (e.g. modules on system.img)
    #[default]
    Local,
    Vendor,
    System,

    /// A Vendor Interface Object, which promises to be stable
    Vintf,
}

impl From<Stability> for i32 {
    fn from(stability: Stability) -> i32 {
        use Stability::*;
        match stability {
            Local => 0,
            Vendor => 0b000011,
            System => 0b001100,
            Vintf => 0b111111,
        }
    }
}

impl TryFrom<i32> for Stability {
    type Error = StatusCode;
    fn try_from(stability: i32) -> Result<Stability> {
        use Stability::*;
        match stability {
            stability if stability == Stability::Local.into() => Ok(Local),
            stability if stability == Stability::Vendor.into() => Ok(Vendor),
            stability if stability == Stability::System.into() => Ok(System),
            stability if stability == Stability::Vintf.into() => Ok(Vintf),
            _ => {
                log::error!("Stability value is invalid: {}", stability);
                Err(StatusCode::BadValue)
            }
        }
    }
}

const INITIAL_STRONG_VALUE: i32 = i32::MAX as _;

#[derive(Debug)]
struct Inner {
    strong: AtomicI32,
    weak: AtomicI32,
    is_strong_lifetime: AtomicBool,
    data: Box<dyn IBinder>,
    descriptor: String,
}

impl Inner {
    fn new(data: Box<dyn IBinder>, is_strong: bool, descriptor: &str) -> Arc<Self> {
        Arc::new(
            Self {
                strong: AtomicI32::new(INITIAL_STRONG_VALUE),
                weak: AtomicI32::new(1),
                is_strong_lifetime: AtomicBool::new(is_strong),
                data,
                descriptor: descriptor.to_owned(),
            }
        )
    }
}

impl Drop for Inner {
    fn drop(self: &mut Inner) {
        let strong = self.strong.load(Ordering::Relaxed);
        let weak = self.weak.load(Ordering::Relaxed);
        if (strong != 0 && strong != INITIAL_STRONG_VALUE) || weak != 0 {
            log::error!("The Drop of Inner IBinder was called with strong count({strong}) and weak count({weak}).");
        }
        if let Some(proxy) = self.data.as_proxy() {
            thread_state::dec_weak_handle(proxy.handle())
                .expect("Failed to decrease the binder weak reference count.")
        }
    }
}

/// Strong reference to a binder object.
#[derive(Debug)]
pub struct SIBinder {
    inner: Arc<Inner>,
}

impl SIBinder {
    pub fn new(data: Box<dyn IBinder>, descriptor: &str) -> Result<Self> {
        WIBinder::new(data, descriptor)?.upgrade()
    }

    fn new_with_inner(inner: Arc<Inner>) -> Result<Self> {
        let this = Self { inner };
        this.increase()?;
        Ok(this)
    }

    pub fn downgrade(this: &Self) -> WIBinder {
        WIBinder::new_with_inner(this.inner.clone())
        // drop will be called.
    }

    pub fn descriptor(&self) -> &str {
        &self.inner.descriptor
    }

    // pub fn stability(&self) -> Stability {
    //     self.inner.data.stability()
    // }

    pub(crate) fn increase(&self) -> Result<()> {
        // In the Android implementation, it simultaneously increases the weak reference,
        // but until the necessity is confirmed, we will not support the related functionality here.
        let c = self.inner.strong.fetch_add(1, Ordering::Relaxed);
        if c == INITIAL_STRONG_VALUE {
            self.inner.strong.fetch_sub(INITIAL_STRONG_VALUE, Ordering::Relaxed);
            if let Some(proxy) = self.inner.data.as_proxy() {
                thread_state::inc_strong_handle(proxy.handle(), self.clone())?;
            }
        }

        Ok(())
    }

    pub(crate) fn attempt_increase(&self) -> bool {
        let mut curr_count = self.inner.strong.load(Ordering::Relaxed);
        debug_assert!(curr_count >= 0, "attempt_increase called on [{}] after underflow", self.descriptor());
        while curr_count > 0 && curr_count != INITIAL_STRONG_VALUE {
            match self.inner.strong.compare_exchange_weak(curr_count, curr_count + 1,
                Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(count) => curr_count = count,
            }
        }

        if curr_count <= 0 || curr_count == INITIAL_STRONG_VALUE {
            if self.inner.is_strong_lifetime.load(Ordering::Relaxed) {
                if curr_count <= 0 {
                    return false;
                }
                while curr_count > 0 {
                    match self.inner.strong.compare_exchange_weak(curr_count, curr_count + 1,
                        Ordering::Relaxed, Ordering::Relaxed) {
                        Ok(_) => break,
                        Err(count) => curr_count = count,
                    }
                }
                if curr_count <= 0 {
                    return false;
                }
            } else {
                if let Some(proxy) = self.inner.data.as_proxy() {
                    if let Err(err) = thread_state::attempt_inc_strong_handle(proxy.handle()) {
                        log::error!("Error in attempt_inc_strong_handle() is {:?}", err);
                        return false;
                    }
                }
                curr_count = self.inner.strong.fetch_add(1, Ordering::Relaxed);
                if curr_count != 0 && curr_count != INITIAL_STRONG_VALUE {
                    if let Some(proxy) = self.inner.data.as_proxy() {
                        thread_state::dec_strong_handle(proxy.handle())
                            .expect("Failed to decrease the binder strong reference count.");
                    }
                }
            }
        }
        if curr_count == INITIAL_STRONG_VALUE {
            self.inner.strong.fetch_sub(INITIAL_STRONG_VALUE, Ordering::Relaxed);
        }

        true
    }

    pub(crate) fn decrease(&self) -> Result<()> {
        let c = self.inner.strong.fetch_sub(1, Ordering::Relaxed);
        if c == 1 {
            if let Some(proxy) = self.inner.data.as_proxy() {
                thread_state::dec_strong_handle(proxy.handle())?;
            }
            self.inner.strong.compare_exchange(0, INITIAL_STRONG_VALUE,
                Ordering::Relaxed, Ordering::Relaxed)
                .expect("Failed to exchange the strong reference count.");
        }

        Ok(())
    }


    /// Try to convert this Binder object into a trait object for the given
    /// Binder interface.
    ///
    /// If this object does not implement the expected interface, the error
    /// `StatusCode::BAD_TYPE` is returned.
    pub fn into_interface<I: FromIBinder + Interface + ?Sized>(self) -> Result<Strong<I>> {
        FromIBinder::try_from(self)
    }
}

impl Clone for SIBinder {
    fn clone(&self) -> Self {
        Self::new_with_inner(self.inner.clone()).unwrap()
    }
}

impl Drop for SIBinder {
    fn drop(&mut self) {
        self.decrease().map_err(|err| {
            log::error!("Error in SIBinder::drop() is {:?}", err);
        }).ok();
    }
}

impl Deref for SIBinder {
    type Target = Box<dyn IBinder>;
    fn deref(&self) -> &Self::Target {
        &self.inner.data
    }
}

impl PartialEq for SIBinder {
    fn eq(&self, other: &Self) -> bool {
        self.inner.data.id() == other.inner.data.id()
    }
}

impl Eq for SIBinder {}

/// Weak reference to a binder object.
#[derive(Debug)]
pub struct WIBinder {
    inner: Arc<Inner>,
}

impl WIBinder {
    pub(crate) fn new(data: Box<dyn IBinder>, descriptor: &str) -> Result<Self> {
        match data.as_proxy().map(|proxy| proxy.handle()) {
            Some(handle) => {
                let this = Self { inner: Inner::new(data, false, descriptor) };
                thread_state::inc_weak_handle(handle, this.clone())?;
                Ok(this)
            }
            None => {
                Ok(Self { inner: Inner::new(data, true, descriptor) })
            }
        }
        // Don't increase the weak reference count. Because the weak reference count is initialized to 1.
    }

    fn new_with_inner(inner: Arc<Inner>) -> Self {
        let this = Self { inner };
        this.increase();
        this
    }

    pub(crate) fn increase(&self) {
        self.inner.weak.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn decrease(&self) {
        self.inner.weak.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn upgrade(&self) -> Result<SIBinder> {
        SIBinder::new_with_inner(self.inner.clone())
    }
}

impl Clone for WIBinder {
    fn clone(&self) -> Self {
        Self::new_with_inner(self.inner.clone())
    }
}

impl Drop for WIBinder {
    fn drop(&mut self) {
        self.decrease();
    }
}

impl Deref for WIBinder {
    type Target = Box<dyn IBinder>;
    fn deref(&self) -> &Self::Target {
        &self.inner.data
    }
}

impl PartialEq for WIBinder {
    fn eq(&self, other: &Self) -> bool {
        self.inner.data.id() == other.inner.data.id()
    }
}


/// Strong reference to a binder object
pub struct Strong<I: FromIBinder + ?Sized>(Box<I>);

impl<I: FromIBinder + ?Sized> Strong<I> {
    /// Create a new strong reference to the provided binder object
    pub fn new(binder: Box<I>) -> Self {
        Self(binder)
    }

    /// Construct a new weak reference to this binder
    pub fn downgrade(this: &Strong<I>) -> Weak<I> {
        Weak::new(this)
    }
}

impl<I: FromIBinder + ?Sized> Clone for Strong<I> {
    fn clone(&self) -> Self {
        // Since we hold a strong reference, we should always be able to create
        // a new strong reference to the same interface type, so try_from()
        // should never fail here.
        FromIBinder::try_from(self.0.as_binder()).unwrap()
    }
}

impl<I: FromIBinder + ?Sized> Borrow<I> for Strong<I> {
    fn borrow(&self) -> &I {
        &self.0
    }
}

impl<I: FromIBinder + ?Sized> AsRef<I> for Strong<I> {
    fn as_ref(&self) -> &I {
        &self.0
    }
}

impl<I: FromIBinder + ?Sized> Deref for Strong<I> {
    type Target = I;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<I: FromIBinder + Debug + ?Sized> Debug for Strong<I> {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        Debug::fmt(&**self, f)
    }
}

impl<I: FromIBinder + ?Sized> PartialEq for Strong<I> {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_binder().eq(&other.0.as_binder())
    }
}

impl<I: FromIBinder + ?Sized> Eq for Strong<I> {}

/// Weak reference to a binder object
#[derive(Debug)]
pub struct Weak<I: FromIBinder + ?Sized> {
    weak_binder: WIBinder,
    interface_type: PhantomData<I>,
}

impl<I: FromIBinder + ?Sized> Weak<I> {
    /// Construct a new weak reference from a strong reference
    fn new(binder: &Strong<I>) -> Self {
        let weak_binder = SIBinder::downgrade(&binder.as_binder());
        Weak {
            weak_binder,
            interface_type: PhantomData,
        }
    }

    /// Upgrade this weak reference to a strong reference if the binder object
    /// is still alive
    pub fn upgrade(&self) -> Result<Strong<I>> {
        self.weak_binder.upgrade().and_then(FromIBinder::try_from)
    }
}

impl<I: FromIBinder + ?Sized> Clone for Weak<I> {
    fn clone(&self) -> Self {
        Self {
            weak_binder: self.weak_binder.clone(),
            interface_type: PhantomData,
        }
    }
}

impl<I: FromIBinder + ?Sized> PartialEq for Weak<I> {
    fn eq(&self, other: &Self) -> bool {
        self.weak_binder == other.weak_binder
    }
}

impl<I: FromIBinder + ?Sized> Eq for Weak<I> {}


#[cfg(test)]
mod tests {
    // use crate::proxy::ProxyHandle;
    use super::*;

    #[test]
    fn test_strong() -> Result<()> {
        // let descriptor = "interface";
        // let strong = SIBinder::new(Box::new(ProxyHandle::new(0, descriptor, Default::default())), descriptor);
        // assert_eq!(strong.inner.strong.load(Ordering::Relaxed), 1);

        // let strong2 = strong.clone();
        // assert_eq!(strong2.inner.strong.load(Ordering::Relaxed), 2);


        // let weak = SIBinder::downgrade(&strong);

        // assert_eq!(weak.inner.strong.load(Ordering::Relaxed), 1);

        // let strong = weak.upgrade();
        // assert_eq!(strong.inner.strong.load(Ordering::Relaxed), 2);
        // SIBinder::downgrade(&strong);
        // assert_eq!(*strong2.0.lock().unwrap(), 101);

        // let weak = strong2.downgrade();

        // assert_eq!(*weak.0.lock().unwrap(), 1);

        Ok(())
    }
}
