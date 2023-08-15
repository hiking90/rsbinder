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

use crate::{
    IBinder,
    sys::*,
    error::*,
    process_state::*,
    proxy::*,
    binder::*,
    parcel::Parcel,
};


/// Super-trait for Binder parcelables.
///
/// This trait is equivalent `android::Parcelable` in C++,
/// and defines a common interface that all parcelables need
/// to implement.
pub trait Parcelable {
    /// Internal serialization function for parcelables.
    ///
    /// This method is mainly for internal use.
    /// `Serialize::serialize` and its variants are generally
    /// preferred over this function, since the former also
    /// prepend a header.
    fn write_to_parcel(&self, parcel: &mut Parcel) -> Result<()>;

    /// Internal deserialization function for parcelables.
    ///
    /// This method is mainly for internal use.
    /// `Deserialize::deserialize` and its variants are generally
    /// preferred over this function, since the former also
    /// parse the additional header.
    fn read_from_parcel(&mut self, parcel: &mut Parcel) -> Result<()>;
}


/// Metadata that `ParcelableHolder` needs for all parcelables.
///
/// The compiler auto-generates implementations of this trait
/// for AIDL parcelables.
pub trait ParcelableMetadata {
    /// The Binder parcelable descriptor string.
    ///
    /// This string is a unique identifier for a Binder parcelable.
    fn get_descriptor() -> &'static str;

    /// The Binder parcelable stability.
    fn get_stability(&self) -> Stability {
        Stability::Local
    }
}

/// A struct whose instances can be written to a [`Parcel`].
// Might be able to hook this up as a serde backend in the future?
pub trait Serialize {
    /// Serialize this instance into the given [`Parcel`].
    fn serialize(&self, parcel: &mut Parcel) -> Result<()>;
}

/// A struct whose instances can be restored from a [`Parcel`].
// Might be able to hook this up as a serde backend in the future?
pub trait Deserialize: Sized {
    /// Deserialize an instance from the given [`Parcel`].
    fn deserialize(parcel: &mut Parcel) -> Result<Self>;

    /// Deserialize an instance from the given [`Parcel`] onto the
    /// current object. This operation will overwrite the old value
    /// partially or completely, depending on how much data is available.
    fn deserialize_from(&mut self, parcel: &mut Parcel) -> Result<()> {
        *self = Self::deserialize(parcel)?;
        Ok(())
    }
}


// /// Helper trait for types that can be serialized as arrays.
// /// Defaults to calling Serialize::serialize() manually for every element,
// /// but can be overridden for custom implementations like `writeByteArray`.
// // Until specialization is stabilized in Rust, we need this to be a separate
// // trait because it's the only way to have a default implementation for a method.
// // We want the default implementation for most types, but an override for
// // a few special ones like `readByteArray` for `u8`.
// pub trait SerializeArray: Serialize + Sized {
//     /// Serialize an array of this type into the given parcel.
//     fn serialize_array(slice: &[Self], parcel: &mut BorrowedParcel<'_>) -> Result<()> {
//         let res = unsafe {
//             // Safety: Safe FFI, slice will always be a safe pointer to pass.
//             sys::AParcel_writeParcelableArray(
//                 parcel.as_native_mut(),
//                 slice.as_ptr() as *const c_void,
//                 slice.len().try_into().or(Err(StatusCode::BAD_VALUE))?,
//                 Some(serialize_element::<Self>),
//             )
//         };
//         status_result(res)
//     }
// }

// /// Helper trait for types that can be deserialized as arrays.
// /// Defaults to calling Deserialize::deserialize() manually for every element,
// /// but can be overridden for custom implementations like `readByteArray`.
// pub trait DeserializeArray: Deserialize {
//     /// Deserialize an array of type from the given parcel.
//     fn deserialize_array(parcel: &BorrowedParcel<'_>) -> Result<Option<Vec<Self>>> {
//         let mut vec: Option<Vec<MaybeUninit<Self>>> = None;
//         let res = unsafe {
//             // Safety: Safe FFI, vec is the correct opaque type expected by
//             // allocate_vec and deserialize_element.
//             sys::AParcel_readParcelableArray(
//                 parcel.as_native(),
//                 &mut vec as *mut _ as *mut c_void,
//                 Some(allocate_vec::<Self>),
//                 Some(deserialize_element::<Self>),
//             )
//         };
//         status_result(res)?;
//         let vec: Option<Vec<Self>> = unsafe {
//             // Safety: We are assuming that the NDK correctly initialized every
//             // element of the vector by now, so we know that all the
//             // MaybeUninits are now properly initialized. We can transmute from
//             // Vec<MaybeUninit<T>> to Vec<T> because MaybeUninit<T> has the same
//             // alignment and size as T, so the pointer to the vector allocation
//             // will be compatible.
//             mem::transmute(vec)
//         };
//         Ok(vec)
//     }
// }


// /// Helper trait for types that can be nullable when serialized.
// // We really need this trait instead of implementing `Serialize for Option<T>`
// // because of the Rust orphan rule which prevents us from doing
// // `impl Serialize for Option<&dyn IFoo>` for AIDL interfaces.
// // Instead we emit `impl SerializeOption for dyn IFoo` which is allowed.
// // We also use it to provide a default implementation for AIDL-generated
// // parcelables.
// pub trait SerializeOption: Serialize {
//     /// Serialize an Option of this type into the given parcel.
//     fn serialize_option(this: Option<&Self>, parcel: &mut BorrowedParcel<'_>) -> Result<()> {
//         if let Some(inner) = this {
//             parcel.write(&NON_NULL_PARCELABLE_FLAG)?;
//             parcel.write(inner)
//         } else {
//             parcel.write(&NULL_PARCELABLE_FLAG)
//         }
//     }
// }

// /// Helper trait for types that can be nullable when deserialized.
// pub trait DeserializeOption: Deserialize {
//     /// Deserialize an Option of this type from the given parcel.
//     fn deserialize_option(parcel: &BorrowedParcel<'_>) -> Result<Option<Self>> {
//         let null: i32 = parcel.read()?;
//         if null == NULL_PARCELABLE_FLAG {
//             Ok(None)
//         } else {
//             parcel.read().map(Some)
//         }
//     }

//     /// Deserialize an Option of this type from the given parcel onto the
//     /// current object. This operation will overwrite the current value
//     /// partially or completely, depending on how much data is available.
//     fn deserialize_option_from(this: &mut Option<Self>, parcel: &BorrowedParcel<'_>) -> Result<()> {
//         *this = Self::deserialize_option(parcel)?;
//         Ok(())
//     }
// }

macro_rules! parcelable_primitives {
    {
        $(
            impl $trait:ident for $ty:ty;
        )*
    } => {
        $(impl_parcelable!{$trait, $ty})*
    };
}

macro_rules! impl_parcelable {
    {Serialize, $ty:ty} => {
        impl Serialize for $ty {
            fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
                parcel.write_data(&self.to_ne_bytes());
                Ok(())
            }
        }
    };

    {Deserialize, $ty:ty} => {
        impl Deserialize for $ty {
            fn deserialize(parcel: &mut Parcel) -> Result<Self> {
                Ok(<$ty>::from_ne_bytes(parcel.try_into()?))
            }
        }
    };

    {SerializeArray, $ty:ty} => {
        impl SerializeArray for $ty {
            // fn serialize_array(slice: &[Self], parcel: &mut Parcel) -> Result<()> {
            //     let status = unsafe {
            //         // Safety: `Parcel` always contains a valid pointer to an
            //         // `AParcel`. If the slice is > 0 length, `slice.as_ptr()`
            //         // will be a valid pointer to an array of elements of type
            //         // `$ty`. If the slice length is 0, `slice.as_ptr()` may be
            //         // dangling, but this is safe since the pointer is not
            //         // dereferenced if the length parameter is 0.
            //         $write_array_fn(
            //             parcel.as_native_mut(),
            //             slice.as_ptr(),
            //             slice
            //                 .len()
            //                 .try_into()
            //                 .or(Err(StatusCode::BAD_VALUE))?,
            //         )
            //     };
            //     status_result(status)
            // }
        }
    };

    {DeserializeArray, $ty:ty} => {
        impl DeserializeArray for $ty {
            // fn deserialize_array(parcel: &mut Parcel) -> Result<Vec<Self>> {
            //     let mut vec: Option<Vec<MaybeUninit<Self>>> = None;
            //     let status = unsafe {
            //         // Safety: `Parcel` always contains a valid pointer to an
            //         // `AParcel`. `allocate_vec<T>` expects the opaque pointer to
            //         // be of type `*mut Option<Vec<MaybeUninit<T>>>`, so `&mut vec` is
            //         // correct for it.
            //         $read_array_fn(
            //             parcel.as_native(),
            //             &mut vec as *mut _ as *mut c_void,
            //             Some(allocate_vec_with_buffer),
            //         )
            //     };
            //     status_result(status)?;
            //     let vec: Option<Vec<Self>> = unsafe {
            //         // Safety: We are assuming that the NDK correctly
            //         // initialized every element of the vector by now, so we
            //         // know that all the MaybeUninits are now properly
            //         // initialized.
            //         vec.map(|vec| vec_assume_init(vec))
            //     };
            //     Ok(vec)
            // }
        }
    };
}

macro_rules! parcelable_primitives_ex {
    {
        $(
            impl $trait:ident for $ty:ty = $to_ty:ty;
        )*
    } => {
        $(impl_parcelable_ex!{$trait, $to_ty, $ty})*

    };
}

macro_rules! impl_parcelable_ex {
    {Serialize, $to_ty:ty, $ty:ty} => {
        impl Serialize for $ty {
            fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
                let val: $to_ty = *self as _;
                parcel.write_data(&val.to_ne_bytes());
                Ok(())
            }
        }
    };

    {Deserialize, $to_ty:ty, $ty:ty} => {
        impl Deserialize for $ty {
            fn deserialize(parcel: &mut Parcel) -> Result<Self> {
                Ok(<$to_ty>::from_ne_bytes(parcel.try_into()?) as _)
            }
        }
    };

    {SerializeArray, $to_ty:ty, $ty:ty} => {
        impl SerializeArray for $ty {
        }
    };

    {DeserializeArray, $to_ty:ty, $ty:ty} => {
        impl DeserializeArray for $ty {
        }
    };
}


parcelable_primitives! {
    impl SerializeArray for i8;
    impl DeserializeArray for i8;

    impl SerializeArray for u8;
    impl DeserializeArray for u8;

    impl SerializeArray for i16;
    impl DeserializeArray for i16;

    impl SerializeArray for u16;
    impl DeserializeArray for u16;


    impl Serialize for i32;
    impl Deserialize for i32;
    impl SerializeArray for i32;
    impl DeserializeArray for i32;

    impl Serialize for u32;
    impl Deserialize for u32;
    impl SerializeArray for u32;
    impl DeserializeArray for u32;

    impl Serialize for f32;
    impl Deserialize for f32;
    impl SerializeArray for f32;
    impl DeserializeArray for f32;

    impl Serialize for i64;
    impl Deserialize for i64;
    impl SerializeArray for i64;
    impl DeserializeArray for i64;

    impl Serialize for u64;
    impl Deserialize for u64;
    impl SerializeArray for u64;
    impl DeserializeArray for u64;

    impl Serialize for f64;
    impl Deserialize for f64;
    impl SerializeArray for f64;
    impl DeserializeArray for f64;

    impl Serialize for u128;
    impl Deserialize for u128;
    impl SerializeArray for u128;
    impl DeserializeArray for u128;
}

parcelable_primitives_ex! {
    impl Serialize for i8 = i32;
    impl Deserialize for i8 = i32;

    impl Serialize for u8 = i32;
    impl Deserialize for u8 = i32;

    impl Serialize for i16 = i32;
    impl Deserialize for i16 = i32;

    impl Serialize for u16 = u32;
    impl Deserialize for u16 = u32;
}

impl Deserialize for bool {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Ok(<i32>::from_ne_bytes(parcel.try_into()?) != 0)
    }
}

impl Serialize for bool {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        let val: i32 = *self as _;
        parcel.write_data(&val.to_ne_bytes());
        Ok(())
    }
}

impl Serialize for str {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write(&String16(self.to_owned()))
        // parcel.write_data(self.as_bytes());
        // Ok(())
    }
}


macro_rules! parcelable_struct {
    {
        $(
            impl $trait:ident for $ty:ty;
        )*
    } => {
        $(impl_parcelable_struct!{$trait, $ty})*
    };
}

macro_rules! impl_parcelable_struct {
    {Serialize, $ty:ty} => {
        impl Serialize for $ty {
            fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
                const SIZE: usize = std::mem::size_of::<$ty>();
                parcel.write_data(unsafe { std::mem::transmute::<&$ty, &[u8;SIZE]>(self) });
                Ok(())
            }
        }
    };

    {Deserialize, $ty:ty} => {
        impl Deserialize for $ty {
            fn deserialize(parcel: &mut Parcel) -> Result<Self> {
                const SIZE: usize = std::mem::size_of::<$ty>();
                Ok(unsafe { std::mem::transmute::<[u8; SIZE], $ty>(parcel.try_into()?) })
            }
        }
    };
}

parcelable_struct! {
    impl Serialize for binder_transaction_data_secctx;
    impl Deserialize for binder_transaction_data_secctx;

    impl Serialize for binder_transaction_data;
    impl Deserialize for binder_transaction_data;
}

impl Serialize for String {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        self.as_str().serialize(parcel)
    }
}

impl SerializeArray for String {}

impl Deserialize for Option<String> {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let res = parcel.read::<Option<String16>>()?.map(|s| s.0);
        Ok(res)
    }
}

impl Deserialize for String {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Deserialize::deserialize(parcel)
            .transpose()
            .unwrap_or(Err(StatusCode::UnexpectedNull.into()))
    }
}

impl DeserializeArray for String {}

#[derive(PartialEq, Debug, Default)]
pub struct String16(pub String);

impl Serialize for String16 {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        let mut utf16 = Vec::with_capacity(self.0.len() + 1);

        for ch16 in self.0.encode_utf16() {
            utf16.push(ch16);
        }

        parcel.write::<i32>(&(utf16.len() as i32))?;

        utf16.push(0);

        let pad_size = crate::parcel::pad_size(utf16.len() * std::mem::size_of::<u16>());
        parcel.write_data(
            unsafe {
                std::slice::from_raw_parts(utf16.as_ptr() as *const u8, pad_size)
            }
        );

        Ok(())
    }
}

impl Deserialize for Option<String16> {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let len = parcel.read::<i32>()?;

        if len >= 0 && len < i32::MAX {
            let pad_size = crate::parcel::pad_size((len as usize + 1) * std::mem::size_of::<u16>());

            let data = parcel.read_data(pad_size)?;
            let res = String::from_utf16(
                unsafe {
                    std::slice::from_raw_parts(data.as_ptr() as *const u16, len as _)
                }
            )?;

            Ok(Some(String16(res)))
        } else {
            Ok(None)
        }
    }
}

impl Deserialize for String16 {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Deserialize::deserialize(parcel)
            .transpose()
            .unwrap_or(Err(StatusCode::UnexpectedNull.into()))
    }
}

impl Deserialize for flat_binder_object {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        parcel.read_object(false)
    }
}

impl Serialize for flat_binder_object {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write_object(self, false)?;
        Ok(())
    }
}

impl Deserialize for *const dyn IBinder {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let data = parcel.read::<u128>()?;
        let ptr = unsafe {std::mem::transmute::<u128, *const dyn IBinder>(data)};
        Ok(ptr)
    }
}

impl Serialize for *const dyn IBinder {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        let data = unsafe {std::mem::transmute::<&*const dyn IBinder, &u128>(self)};
        parcel.write::<u128>(data)?;
        Ok(())
    }
}


const SCHED_NORMAL:u32 = 0;
const FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT:u32 = 9;

fn sched_policy_mask(policy: u32, priority: u32) -> u32 {
    (priority & FLAT_BINDER_FLAG_PRIORITY_MASK) | ((policy & 3) << FLAT_BINDER_FLAG_SCHED_POLICY_SHIFT)
}

impl Serialize for StrongIBinder {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {

        let sched_bits = if ProcessState::as_self().background_scheduling_disabled() == false {
            sched_policy_mask(SCHED_NORMAL, 19)
        } else {
            0
        };

        let obj = if self.is_remote() {
            let proxy = self.as_any().downcast_ref::<ProxyHandle>().expect("Downcast to Proxy<Unknown>");

            flat_binder_object {
                hdr: binder_object_header {
                    type_: BINDER_TYPE_HANDLE
                },
                flags: sched_bits,
                __bindgen_anon_1: flat_binder_object__bindgen_ty_1 {
                    handle: proxy.handle(),
                },
                cookie: 0,
            }
        } else {
            let weak = Box::new(StrongIBinder::downgrade(self));

            flat_binder_object {
                hdr: binder_object_header {
                    type_: BINDER_TYPE_BINDER
                },
                flags: FLAT_BINDER_FLAG_ACCEPTS_FDS | sched_bits,
                __bindgen_anon_1: flat_binder_object__bindgen_ty_1 {
                    binder: Box::into_raw(weak) as u64,
                },
                cookie: 0,
            }
        };

        parcel.write(&obj)?;

        // finishFlattenBinder
        let stability: i32 = 0;
        parcel.write(&stability)?;

        Ok(())
    }
}

impl SerializeOption for StrongIBinder {}

impl Deserialize for StrongIBinder {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let flat: flat_binder_object = parcel.read()?;
        let _stability: i32 = parcel.read()?;

        unsafe {
            match flat.hdr.type_ {
                BINDER_TYPE_BINDER => {
                    let weak = Box::from_raw(flat.__bindgen_anon_1.binder as *mut Box<WeakIBinder>);
                    Ok(weak.upgrade())

                    // Weak::upgrade(&weak)
                    //     .ok_or_else(|| Error::from(StatusCode::DeadObject))
                }

                BINDER_TYPE_HANDLE => {
                    let res = ProcessState::as_self()
                        .strong_proxy_for_handle(flat.__bindgen_anon_1.handle);
                    Ok(res?)
                }

                _ => {
                    log::warn!("Unknown Binder Type ({}) was delivered.", flat.hdr.type_);
                    Err(Error::from(StatusCode::BadType))
                }
            }
        }
    }
}

impl DeserializeOption for StrongIBinder {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        Ok(Some(parcel.read()?))
    }
}

// impl DeserializeOption for StrongIBinder {
//     fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
//         if let Some(inner) = this {
//             parcel.write(&NON_NULL_PARCELABLE_FLAG)?;
//             parcel.write(inner)
//         } else {
//             parcel.write(&NULL_PARCELABLE_FLAG)
//         }
//     }
// }

/// Flag that specifies that the following parcelable is present.
///
/// This is the Rust equivalent of `Parcel::kNonNullParcelableFlag`
/// from `include/binder/Parcel.h` in C++.
pub const NON_NULL_PARCELABLE_FLAG: i32 = 1;

/// Flag that specifies that the following parcelable is absent.
///
/// This is the Rust equivalent of `Parcel::kNullParcelableFlag`
/// from `include/binder/Parcel.h` in C++.
pub const NULL_PARCELABLE_FLAG: i32 = 0;

/// Helper trait for types that can be nullable when serialized.
// We really need this trait instead of implementing `Serialize for Option<T>`
// because of the Rust orphan rule which prevents us from doing
// `impl Serialize for Option<&dyn IFoo>` for AIDL interfaces.
// Instead we emit `impl SerializeOption for dyn IFoo` which is allowed.
// We also use it to provide a default implementation for AIDL-generated
// parcelables.
pub trait SerializeOption: Serialize {
    /// Serialize an Option of this type into the given parcel.
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        if let Some(inner) = this {
            parcel.write(&NON_NULL_PARCELABLE_FLAG)?;
            parcel.write(inner)
        } else {
            parcel.write(&NULL_PARCELABLE_FLAG)
        }
    }
}

/// Helper trait for types that can be nullable when deserialized.
pub trait DeserializeOption: Deserialize {
    /// Deserialize an Option of this type from the given parcel.
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        let null: i32 = parcel.read()?;
        if null == NULL_PARCELABLE_FLAG {
            Ok(None)
        } else {
            parcel.read().map(Some)
        }
    }

    /// Deserialize an Option of this type from the given parcel onto the
    /// current object. This operation will overwrite the current value
    /// partially or completely, depending on how much data is available.
    fn deserialize_option_from(this: &mut Option<Self>, parcel: &mut Parcel) -> Result<()> {
        *this = Self::deserialize_option(parcel)?;
        Ok(())
    }
}

impl<T: SerializeOption> Serialize for Option<T> {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        SerializeOption::serialize_option(self.as_ref(), parcel)
    }
}

impl<T: DeserializeOption> Deserialize for Option<T> {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        DeserializeOption::deserialize_option(parcel)
    }

    fn deserialize_from(&mut self, parcel: &mut Parcel) -> Result<()> {
        DeserializeOption::deserialize_option_from(self, parcel)
    }
}

// We need these to support Option<&T> for all T
impl<T: Serialize + ?Sized> Serialize for &T {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        Serialize::serialize(*self, parcel)
    }
}

impl<T: SerializeOption + ?Sized> SerializeOption for &T {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        SerializeOption::serialize_option(this.copied(), parcel)
    }
}

impl<T: Serialize> Serialize for Box<T> {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        Serialize::serialize(&**self, parcel)
    }
}

impl<T: Deserialize> Deserialize for Box<T> {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Deserialize::deserialize(parcel).map(Box::new)
    }
}


/// Helper trait for types that can be serialized as arrays.
/// Defaults to calling Serialize::serialize() manually for every element,
/// but can be overridden for custom implementations like `writeByteArray`.
// Until specialization is stabilized in Rust, we need this to be a separate
// trait because it's the only way to have a default implementation for a method.
// We want the default implementation for most types, but an override for
// a few special ones like `readByteArray` for `u8`.
pub trait SerializeArray: Serialize + Sized {
    /// Serialize an array of this type into the given parcel.
    fn serialize_array(slice: &[Self], parcel: &mut Parcel) -> Result<()> {
        parcel.write::<i32>(&(slice.len() as i32))?;

        for s in slice {
            parcel.write(s)?;
        }

        Ok(())
    }
}


/// Helper trait for types that can be deserialized as arrays.
/// Defaults to calling Deserialize::deserialize() manually for every element,
/// but can be overridden for custom implementations like `readByteArray`.
pub trait DeserializeArray: Deserialize {
    /// Deserialize an array of type from the given parcel.
    fn deserialize_array(parcel: &mut Parcel) -> Result<Vec<Self>> {
        let len: i32 = parcel.read()?;
        if len < 0 {
            return Err(StatusCode::BadValue.into());
        }
        let mut res: Vec<Self> = Vec::with_capacity(len as _);

        for _ in 0..len {
            res.push(parcel.read()?);
        }

        Ok(res)
    }
}

impl<T: SerializeArray> Serialize for [T] {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        SerializeArray::serialize_array(self, parcel)
    }
}

impl<T: SerializeArray> Serialize for Vec<T> {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        SerializeArray::serialize_array(&self[..], parcel)
    }
}

impl<T: DeserializeArray> Deserialize for Vec<T> {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        DeserializeArray::deserialize_array(parcel)
    }
}

// impl<T: SerializeArray, const N: usize> SerializeArray for [T; N] {}

impl<T: DeserializeArray, const N: usize> Deserialize for [T; N] {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let vec = DeserializeArray::deserialize_array(parcel)?;
        vec.try_into().or(Err(StatusCode::BadValue.into()))
    }
}

// impl<T: SerializeOption> SerializeOption for Box<T> {
//     fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
//         SerializeOption::serialize_option(this.map(|inner| &**inner), parcel)
//     }
// }

// impl<T: DeserializeOption> DeserializeOption for Box<T> {
//     fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
//         DeserializeOption::deserialize_option(parcel).map(|t| t.map(Box::new))
//     }
// }


// impl Deserialize for binder_transaction_data_secctx {
//     fn deserialize(parcel: &ReadableParcel<'_>) -> Result<Self> {
//         const SIZE: usize = std::mem::size_of::<binder_transaction_data_secctx>();
//         Ok(unsafe { std::mem::transmute::<[u8; SIZE], Self>(parcel.try_into()?) })
//     }
// }

// impl Deserialize for binder_transaction_data {
//     fn deserialize(parcel: &ReadableParcel<'_>) -> Result<Self> {
//         todo!("Deserialize for binder::binder_transaction_data")
//         // Ok(<i32>::from_ne_bytes(parcel.try_into()?) != 0)
//     }
// }