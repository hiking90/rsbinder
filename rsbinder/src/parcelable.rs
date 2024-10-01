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
    sys::*,
    error::*,
    process_state::*,
    binder::*,
    parcel::Parcel,
    binder_object::*,
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
    fn descriptor() -> &'static str;

    /// The Binder parcelable stability.
    fn stability(&self) -> Stability {
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
                parcel.write_aligned(self);
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
            fn serialize_array(slice: &[Self], parcel: &mut Parcel) -> Result<()> {
                parcel.write_array(slice)
            }
        }
    };

    {DeserializeArray, $ty:ty} => {
        impl DeserializeArray for $ty {
            fn deserialize_array(parcel: &mut Parcel) -> Result<Option<Vec<Self>>> {
                parcel.read_array()
            }
        }
    };
    {SerializeOption, $ty:ty} => {
        impl SerializeOption for $ty {
        }
    };

    {DeserializeOption, $ty:ty} => {
        impl DeserializeOption for $ty {
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
                parcel.write_aligned(&val);
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
}


parcelable_primitives! {
    impl SerializeArray for i8;
    impl DeserializeArray for i8;
    impl SerializeOption for i8;
    impl DeserializeOption for i8;

    impl SerializeArray for u8;
    impl DeserializeArray for u8;
    impl SerializeOption for u8;
    impl DeserializeOption for u8;

    impl SerializeOption for i16;
    impl DeserializeOption for i16;

    impl SerializeOption for u16;
    impl DeserializeOption for u16;

    impl Serialize for i32;
    impl Deserialize for i32;
    impl SerializeArray for i32;
    impl DeserializeArray for i32;
    impl SerializeOption for i32;
    impl DeserializeOption for i32;

    impl Serialize for u32;
    impl Deserialize for u32;
    impl SerializeArray for u32;
    impl DeserializeArray for u32;
    impl SerializeOption for u32;
    impl DeserializeOption for u32;

    impl Serialize for f32;
    impl Deserialize for f32;
    impl SerializeArray for f32;
    impl DeserializeArray for f32;
    impl SerializeOption for f32;
    impl DeserializeOption for f32;

    impl Serialize for i64;
    impl Deserialize for i64;
    impl SerializeArray for i64;
    impl DeserializeArray for i64;
    impl SerializeOption for i64;
    impl DeserializeOption for i64;

    impl Serialize for u64;
    impl Deserialize for u64;
    impl SerializeArray for u64;
    impl DeserializeArray for u64;
    impl SerializeOption for u64;
    impl DeserializeOption for u64;

    impl Serialize for f64;
    impl Deserialize for f64;
    impl SerializeArray for f64;
    impl DeserializeArray for f64;
    impl SerializeOption for f64;
    impl DeserializeOption for f64;

    impl Serialize for u128;
    impl Deserialize for u128;
    impl SerializeArray for u128;
    impl DeserializeArray for u128;
    impl SerializeOption for u128;
    impl DeserializeOption for u128;
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

impl SerializeArray for i16 {
    fn serialize_array(slice: &[Self], parcel: &mut Parcel) -> Result<()> {
        parcel.write_array_char(slice)
    }
}

impl DeserializeArray for i16 {
    fn deserialize_array(parcel: &mut Parcel) -> Result<Option<Vec<Self>>> {
        parcel.read_array_char::<Self>()
    }
}

impl SerializeArray for u16 {
    fn serialize_array(slice: &[Self], parcel: &mut Parcel) -> Result<()> {
        parcel.write_array_char(slice)
    }
}

impl DeserializeArray for u16 {
    fn deserialize_array(parcel: &mut Parcel) -> Result<Option<Vec<Self>>> {
        parcel.read_array_char::<Self>()
    }
}

impl Deserialize for bool {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Ok(<i32>::from_ne_bytes(parcel.try_into()?) != 0)
    }
}

impl DeserializeArray for bool {
}

impl Serialize for bool {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        let val: i32 = *self as _;
        parcel.write_aligned(&val);
        Ok(())
    }
}

impl SerializeArray for bool {
}

impl SerializeOption for str {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        match this {
            None => {
                parcel.write::<i32>(&-1)
            }

            Some(text) => {
                let mut utf16 = Vec::with_capacity(text.len() * 3);   // Due to surrogate pair.
                utf16.extend(text.encode_utf16());

                let len = utf16.len();

                utf16.push(0);

                parcel.write::<i32>(&(len as i32))?;
                let pad_size = crate::parcel::pad_size(utf16.len() * std::mem::size_of::<u16>());
                parcel.write_aligned_data(
                    unsafe {
                        std::slice::from_raw_parts(utf16.as_ptr() as *const u8, pad_size)
                    }
                );

                Ok(())
            }
        }
    }
}

impl Deserialize for StatusCode {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Ok(<i32>::from_ne_bytes(parcel.try_into()?).into())
    }
}

impl Serialize for StatusCode {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        let val: i32 = i32::from(*self);
        parcel.write_aligned(&val);
        Ok(())
    }
}

impl Serialize for str {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        Some(self).serialize(parcel)
    }
}

impl SerializeArray for &str {}

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
                parcel.write_aligned(self);
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

impl SerializeOption for String {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        SerializeOption::serialize_option(this.map(String::as_str), parcel)
    }
}

impl DeserializeOption for String {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        let len = parcel.read::<i32>()?;

        if (0..i32::MAX).contains(&len) {
            let data = parcel.read_aligned_data((len as usize + 1) * std::mem::size_of::<u16>())?;
            let res = String::from_utf16(
                unsafe {
                    std::slice::from_raw_parts(data.as_ptr() as *const u16, len as _)
                }
            ).map_err(|e| {
                log::error!("Deserialize for Option<String16>: {}", e.to_string());
                StatusCode::BadValue
            })?;

            Ok(Some(res))
        } else {
            Ok(None)
        }
    }
}

impl Deserialize for String {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Deserialize::deserialize(parcel)
            .transpose()
            .unwrap_or_else(|| {
                log::error!("Deserialize for String: UnexpectedNull");
                Err(StatusCode::UnexpectedNull)
            })
    }
}

impl DeserializeArray for String {}

impl Deserialize for flat_binder_object {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        parcel.read_object(false).map(|obj| *obj)
    }
}

impl Serialize for flat_binder_object {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write_object(self, false)?;
        Ok(())
    }
}

impl Serialize for SIBinder {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        SerializeOption::serialize_option(Some(self), parcel)
    }
}

impl SerializeOption for SIBinder {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        match this {
            Some(binder) => {
                parcel.write::<flat_binder_object>(&binder.into())?;
                parcel.write::<i32>(&Stability::System.into())?;
                Ok(())
            }

            None => {
                parcel.write::<flat_binder_object>(&flat_binder_object::default())?;
                parcel.write::<i32>(&Stability::Local.into())?;

                Ok(())
            }
        }
    }
}

impl SerializeArray for SIBinder {}

impl Deserialize for SIBinder {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        match DeserializeOption::deserialize_option(parcel) {
            Ok(Some(binder)) => Ok(binder),
            Ok(None) => {
                log::error!("Deserialize for SIBinder: UnexpectedNull");
                Err(StatusCode::UnexpectedNull)
            }
            Err(err) => Err(err),
        }
    }
}

impl DeserializeOption for SIBinder {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        let flat: flat_binder_object = parcel.read()?;
        let stability: i32 = parcel.read()?;

        match flat.header_type() {
            BINDER_TYPE_BINDER => {
                let pointer = flat.pointer();
                if pointer != 0 {
                    let strong = raw_pointer_to_strong_binder((flat.pointer(), flat.cookie()));
                    Ok(Some(SIBinder::clone(&strong)))
                } else {
                    Ok(None)
                }
            }

            BINDER_TYPE_HANDLE => {
                let res = ProcessState::as_self()
                    .strong_proxy_for_handle_stability(flat.handle(), stability.try_into()?)?;
                Ok(Some(res))
            }

            _ => {
                log::warn!("Unknown Binder Type ({}) was delivered.", flat.header_type());
                Err(StatusCode::BadType)
            }
        }
    }
}

impl DeserializeArray for SIBinder {}

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

impl<T: SerializeOption> SerializeOption for Box<T> {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        SerializeOption::serialize_option(this.map(|inner| &**inner), parcel)
    }
}

impl<T: DeserializeOption> DeserializeOption for Box<T> {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        DeserializeOption::deserialize_option(parcel).map(|t| t.map(Box::new))
    }
}

impl<T: FromIBinder + Serialize + ?Sized> Serialize for Strong<T> {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        Serialize::serialize(&**self, parcel)
    }
}

impl<T: FromIBinder + SerializeOption + ?Sized> SerializeOption for Strong<T> {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        SerializeOption::serialize_option(this.map(|b| &**b), parcel)
    }
}

impl<T: FromIBinder + Serialize + ?Sized> SerializeArray for Strong<T> {}

impl<T: FromIBinder + ?Sized> Deserialize for Strong<T> {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let binder: SIBinder = parcel.read()?;
        FromIBinder::try_from(binder)
    }
}

impl<T: FromIBinder + ?Sized> DeserializeOption for Strong<T> {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        let binder: Option<SIBinder> = parcel.read()?;
        binder.map(FromIBinder::try_from).transpose()
    }
}

impl<T: FromIBinder + ?Sized> DeserializeArray for Strong<T> {}

impl<T: DeserializeOption> DeserializeArray for Option<T> {}
impl<T: SerializeOption> SerializeArray for Option<T> {}

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
    fn deserialize_array(parcel: &mut Parcel) -> Result<Option<Vec<Self>>> {
        let len: i32 = parcel.read()?;
        if len < -1 {
            log::error!("Negative array size given in parcel: {}", len);
            return Err(StatusCode::BadValue);
        }
        if len <= 0 {
            return Ok(None)
        }
        let mut res: Vec<Self> = Vec::with_capacity(len as _);

        for _ in 0..len {
            res.push(parcel.read()?);
        }

        Ok(Some(res))
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

impl<T: SerializeArray> SerializeOption for [T] {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        if let Some(v) = this {
            SerializeArray::serialize_array(v, parcel)
        } else {
            parcel.write(&-1i32)
        }
    }
}

impl<T: SerializeArray> SerializeOption for Vec<T> {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        SerializeOption::serialize_option(this.map(Vec::as_slice), parcel)
    }
}


impl<T: DeserializeArray> Deserialize for Vec<T> {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        DeserializeArray::deserialize_array(parcel)
            .map(|v| v.unwrap_or_default())
    }
}

impl<T: DeserializeArray> DeserializeOption for Vec<T> {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        DeserializeArray::deserialize_array(parcel)
    }
}

impl<T: SerializeArray, const N: usize> Serialize for [T; N] {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        SerializeArray::serialize_array(self, parcel)
    }
}

impl<T: SerializeArray, const N: usize> SerializeOption for [T; N] {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        SerializeOption::serialize_option(this.map(|arr| &arr[..]), parcel)
    }
}

impl<T: SerializeArray, const N: usize> SerializeArray for [T; N] {}


impl<T: DeserializeArray, const N: usize> Deserialize for [T; N] {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let vec = DeserializeArray::deserialize_array(parcel)
            .transpose()
            .unwrap_or_else(|| {
                log::error!("Deserialize for [T; N]: UnexpectedNull");
                Err(StatusCode::UnexpectedNull)
            })?;
        vec.try_into()
            .map_err(|_| {
                log::error!("Deserialize: Failed to convert Vec<T> to [T; N]");
                StatusCode::BadValue
            })
    }
}

impl<T: std::fmt::Debug + DeserializeArray, const N: usize> DeserializeOption for [T; N] {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        let vec = DeserializeArray::deserialize_array(parcel)?;
        vec.map(|v| v.try_into()
            .map_err(|_| {
                log::error!("DeserializeOption: Failed to convert Vec<T> to [T; N]");
                StatusCode::BadValue
            }))
            .transpose()
    }
}

impl<T: DeserializeArray, const N: usize> DeserializeArray for [T; N] {}
