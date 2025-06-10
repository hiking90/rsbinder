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

use std::any::Any;
use std::borrow::Borrow;
use std::fmt::{Debug, Formatter};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::sync::{self, Arc};

use crate::{error::*, parcel::*, proxy, sys};

/// Binder action to perform.
///
/// This must be a number between [`FIRST_CALL_TRANSACTION`] and
/// [`LAST_CALL_TRANSACTION`].
pub type TransactionCode = u32;

/// Additional operation flags.
///
/// `FLAG_*` values.
pub type TransactionFlags = u32;

/// Corresponds to TF_ONE_WAY -- an asynchronous call.
pub const FLAG_ONEWAY: TransactionFlags = sys::transaction_flags_TF_ONE_WAY;
/// Corresponds to TF_CLEAR_BUF -- clear transaction buffers after call is made.
pub const FLAG_CLEAR_BUF: TransactionFlags = sys::transaction_flags_TF_CLEAR_BUF;
/// Set to the vendor flag if we are building for the VNDK, 0 otherwise
pub const FLAG_PRIVATE_LOCAL: TransactionFlags = 0;
pub const FLAG_PRIVATE_VENDOR: TransactionFlags = FLAG_PRIVATE_LOCAL;

const fn b_pack_chars(c1: char, c2: char, c3: char, c4: char) -> u32 {
    ((c1 as u32) << 24) | ((c2 as u32) << 16) | ((c3 as u32) << 8) | (c4 as u32)
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

pub const START_RECORDING_TRANSACTION: u32 = b_pack_chars('_', 'S', 'R', 'D');
pub const STOP_RECORDING_TRANSACTION: u32 = b_pack_chars('_', 'E', 'R', 'D');

// See android.os.IBinder.TWEET_TRANSACTION
// Most importantly, messages can be anything not exceeding 130 UTF-8
// characters, and callees should exclaim "jolly good message old boy!"
pub const TWEET_TRANSACTION: u32 = b_pack_chars('_', 'T', 'W', 'T');

// See android.os.IBinder.LIKE_TRANSACTION
// Improve binder self-esteem.
pub const LIKE_TRANSACTION: u32 = b_pack_chars('_', 'L', 'I', 'K');

pub const INTERFACE_HEADER: u32 = b_pack_chars('S', 'Y', 'S', 'T');

/// Super-trait for Binder interfaces.
///
/// This trait allows conversion of a Binder interface trait object into an
/// IBinder object for IPC calls. All Binder remotable interface (i.e. AIDL
/// interfaces) must implement this trait.
///
/// This is equivalent `IInterface` in C++.
pub trait Interface: Send + Sync {
    /// Convert this binder object into a generic [`SIBinder`] reference.
    fn as_binder(&self) -> SIBinder {
        panic!("This object was not a Binder object and cannot be converted into an SIBinder.")
    }

    /// Dump transaction handler for this Binder object.
    ///
    /// This handler is a no-op by default and should be implemented for each
    /// Binder service struct that wishes to respond to dump transactions.
    fn dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        Ok(())
    }
}

/// Trait for converting a generic Binder object into a specific Binder
///
/// # Example
///
/// For Binder interface `IFoo`, the following implementation should be made:
/// ```no_run
/// # use rsbinder::{FromIBinder, SIBinder, Interface, Result, Strong};
/// # trait IFoo : Interface {}
/// impl FromIBinder for dyn IFoo {
///     fn try_from(ibinder: SIBinder) -> Result<Strong<Self>> {
///         // ...
///         # Err(rsbinder::StatusCode::Ok)
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

/// Interface for receiving a notification when a binder object is no longer
/// valid.
///
/// This trait corresponds to the parts of the interface of the C++ `DeathRecipient`
/// class which are public.
pub trait DeathRecipient: Send + Sync {
    fn binder_died(&self, who: &WIBinder);
}

/// Interface of binder local or remote objects.
///
/// This trait corresponds to the parts of the interface of the C++ `IBinder`
/// class which are public.
pub trait IBinder: Any + Send + Sync {
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
    fn link_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()>;

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    fn unlink_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()>;

    /// Send a ping transaction to this object
    fn ping_binder(&self) -> Result<()>;

    /// To support dynamic interface cast, we need to know the interface
    fn as_any(&self) -> &dyn Any;

    /// To convert the interface to a transactable object
    fn as_transactable(&self) -> Option<&dyn Transactable>;

    /// Retrieve the descriptor of this object.
    fn descriptor(&self) -> &str;
    /// Retrieve if this object is remote.
    fn is_remote(&self) -> bool;

    fn inc_strong(&self, strong: &SIBinder) -> Result<()>;
    fn attempt_inc_strong(&self) -> bool;
    fn dec_strong(&self, strong: Option<ManuallyDrop<SIBinder>>) -> Result<()>;
    fn inc_weak(&self, weak: &WIBinder) -> Result<()>;
    fn dec_weak(&self) -> Result<()>;
}

impl dyn IBinder {
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
    fn descriptor() -> &'static str
    where
        Self: Sized;

    /// Handle and reply to a request to invoke a transaction on this object.
    ///
    /// `reply` may be [`None`] if the sender does not expect a reply.
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()>;

    /// Handle a request to invoke the dump transaction on this
    /// object.
    fn on_dump(&self, writer: &mut dyn std::io::Write, args: &[String]) -> Result<()>;
}

/// A transactable object that can be used to process Binder commands.
pub trait Transactable: Send + Sync {
    fn transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()>;
}

/// Implemented by sync interfaces to specify what the associated async interface is.
/// Generic to handle the fact that async interfaces are generic over a thread pool.
///
/// The binder in any object implementing this trait should be compatible with the
/// `Target` associated type, and using `FromIBinder` to convert it to the target
/// should not fail.
pub trait ToAsyncInterface<P>
where
    Self: Interface,
    Self::Target: FromIBinder,
{
    /// The async interface associated with this sync interface.
    type Target: ?Sized;
}

/// Implemented by async interfaces to specify what the associated sync interface is.
///
/// The binder in any object implementing this trait should be compatible with the
/// `Target` associated type, and using `FromIBinder` to convert it to the target
/// should not fail.
pub trait ToSyncInterface
where
    Self: Interface,
    Self::Target: FromIBinder,
{
    /// The sync interface associated with this async interface.
    type Target: ?Sized;
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
    Local,
    Vendor,
    #[default]
    System,

    /// A Vendor Interface Object, which promises to be stable
    Vintf,
}

// Android 12 version uses "Category" as the stability format for passed on the wire lines,
// whereas other versions do not. Therefore, we can use the android_properties crate
// to determine the Android version and perform different handling accordingly.
// http://aospxref.com/android-11.0.0_r21/xref/frameworks/native/libs/binder/include/binder/Stability.h
// http://aospxref.com/android-12.0.0_r3/xref/frameworks/native/include/binder/Stability.h
// http://aospxref.com/android-13.0.0_r3/xref/frameworks/native/libs/binder/include/binder/Stability.h
// http://aospxref.com/android-14.0.0_r2/xref/frameworks/native/libs/binder/include/binder/Stability.h
impl From<Stability> for i32 {
    fn from(stability: Stability) -> i32 {
        use Stability::*;

        let stability = match stability {
            Local => 0,
            Vendor => 0b000011,
            System => 0b001100,
            Vintf => 0b111111,
        };

        #[cfg(target_os = "android")]
        {
            let android_sdk_version = crate::get_android_sdk_version();
            if android_sdk_version == 31 || android_sdk_version == 32 {
                stability | 0x0c000000
            } else {
                stability
            }
        }
        #[cfg(not(target_os = "android"))]
        stability
    }
}

impl TryFrom<i32> for Stability {
    type Error = StatusCode;
    fn try_from(stability: i32) -> Result<Stability> {
        use Stability::*;
        match stability {
            stability if stability == Local.into() => Ok(Local),
            stability if stability == Vendor.into() => Ok(Vendor),
            stability if stability == System.into() => Ok(System),
            stability if stability == Vintf.into() => Ok(Vintf),
            _ => {
                log::error!("Stability value is invalid: {:X}", stability);
                // Err(StatusCode::BadValue)
                Ok(Local)
            }
        }
    }
}

/// Strong reference to a binder object.
pub struct SIBinder {
    inner: Arc<dyn IBinder>,
}

impl SIBinder {
    pub fn new(data: Arc<dyn IBinder>) -> Result<Self> {
        WIBinder::new(data)?.upgrade()
    }

    fn new_with_inner(inner: Arc<dyn IBinder>) -> Result<Self> {
        let this = Self { inner };
        this.increase()?;
        Ok(this)
    }

    pub(crate) fn into_raw(self) -> *const dyn IBinder {
        let inner = Arc::clone(&self.inner);
        let raw = Arc::into_raw(inner);
        std::mem::forget(self);
        raw
    }

    pub(crate) fn from_raw(raw: *const dyn IBinder) -> Self {
        let inner = unsafe { Arc::from_raw(raw) };
        Self { inner }
    }

    pub fn downgrade(this: &Self) -> WIBinder {
        WIBinder::new_with_inner(Arc::clone(&this.inner))
    }

    // pub fn stability(&self) -> Stability {
    //     self.inner.data.stability()
    // }

    pub(crate) fn increase(&self) -> Result<()> {
        self.inner.inc_strong(self)
    }

    pub(crate) fn attempt_increase(&self) -> bool {
        self.inner.attempt_inc_strong()
    }

    pub(crate) fn decrease(&self) -> Result<()> {
        self.inner.dec_strong(None)
    }

    pub(crate) fn decrease_drop(this: ManuallyDrop<Self>) -> Result<()> {
        let inner = Arc::clone(&this.inner);
        inner.dec_strong(Some(this))
    }

    /// Try to convert this Binder object into a trait object for the given
    /// Binder interface.
    ///
    /// If this object does not implement the expected interface, the error
    /// `StatusCode::BadType` is returned.
    pub fn into_interface<I: FromIBinder + Interface + ?Sized>(self) -> Result<Strong<I>> {
        FromIBinder::try_from(self)
    }
}

impl Debug for SIBinder {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        f.debug_struct("SIBinder")
            .field("descriptor", &self.descriptor())
            .field("ptr", &Arc::as_ptr(&self.inner))
            .finish()
    }
}

impl Clone for SIBinder {
    fn clone(&self) -> Self {
        Self::new_with_inner(Arc::clone(&self.inner)).unwrap()
    }
}

impl Drop for SIBinder {
    fn drop(&mut self) {
        self.decrease()
            .map_err(|err| {
                log::error!("Error in SIBinder::drop() is {:?}", err);
            })
            .ok();
    }
}

impl Deref for SIBinder {
    type Target = dyn IBinder;
    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl PartialEq for SIBinder {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for SIBinder {}

/// Weak reference to a binder object.
pub struct WIBinder {
    inner: Arc<dyn IBinder>,
}

impl WIBinder {
    pub(crate) fn new(inner: Arc<dyn IBinder>) -> Result<Self> {
        let this = Self { inner };
        this.inner.inc_weak(&this)?;

        Ok(this)
    }

    fn new_with_inner(inner: Arc<dyn IBinder>) -> Self {
        let this = Self { inner };
        this.increase();
        this
    }

    pub(crate) fn increase(&self) {
        self.inner.inc_weak(self).ok();
    }

    pub(crate) fn decrease(&self) {
        self.inner.dec_weak().ok();
    }

    pub fn upgrade(&self) -> Result<SIBinder> {
        SIBinder::new_with_inner(Arc::clone(&self.inner))
    }
}

impl Debug for WIBinder {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        f.debug_struct("WIBinder")
            .field("self", &(self as *const WIBinder))
            .field("descriptor", &self.inner.descriptor())
            .field("inner_ptr", &Arc::as_ptr(&self.inner))
            .finish()
    }
}

impl Clone for WIBinder {
    fn clone(&self) -> Self {
        Self::new_with_inner(Arc::clone(&self.inner))
    }
}

impl Drop for WIBinder {
    fn drop(&mut self) {
        self.decrease();
    }
}

impl PartialEq for WIBinder {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
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

    /// Convert this synchronous binder handle into an asynchronous one.
    pub fn into_async<P>(self) -> Strong<<I as ToAsyncInterface<P>>::Target>
    where
        I: ToAsyncInterface<P>,
    {
        // By implementing the ToAsyncInterface trait, it is guaranteed that the binder
        // object is also valid for the target type.
        FromIBinder::try_from(self.0.as_binder()).unwrap()
    }

    /// Convert this asynchronous binder handle into a synchronous one.
    pub fn into_sync(self) -> Strong<<I as ToSyncInterface>::Target>
    where
        I: ToSyncInterface,
    {
        // By implementing the ToSyncInterface trait, it is guaranteed that the binder
        // object is also valid for the target type.
        FromIBinder::try_from(self.0.as_binder()).unwrap()
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

    #[test]
    fn test_b_pack_chars() {
        assert_eq!(b_pack_chars('_', 'P', 'N', 'G'), PING_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'D', 'M', 'P'), DUMP_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'C', 'M', 'D'), SHELL_COMMAND_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'N', 'T', 'F'), INTERFACE_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'S', 'P', 'R'), SYSPROPS_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'E', 'X', 'T'), EXTENSION_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'P', 'I', 'D'), DEBUG_PID_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'R', 'P', 'C'), SET_RPC_CLIENT_TRANSACTION);
        assert_eq!(
            b_pack_chars('_', 'S', 'R', 'D'),
            START_RECORDING_TRANSACTION
        );
        assert_eq!(b_pack_chars('_', 'E', 'R', 'D'), STOP_RECORDING_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'T', 'W', 'T'), TWEET_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'L', 'I', 'K'), LIKE_TRANSACTION);
    }

    #[test]
    fn test_stability() {
        assert_eq!(Into::<i32>::into(Stability::Local), 0);
        assert_eq!(Into::<i32>::into(Stability::Vendor), 0b000011);
        assert_eq!(Into::<i32>::into(Stability::System), 0b001100);
        assert_eq!(Into::<i32>::into(Stability::Vintf), 0b111111);

        assert_eq!(Stability::try_from(0).unwrap(), Stability::Local);
        assert_eq!(Stability::try_from(0b000011).unwrap(), Stability::Vendor);
        assert_eq!(Stability::try_from(0b001100).unwrap(), Stability::System);
        assert_eq!(Stability::try_from(0b111111).unwrap(), Stability::Vintf);
        assert_eq!(
            Stability::try_from(0b1111111).unwrap_err(),
            StatusCode::BadValue
        );
    }
}
