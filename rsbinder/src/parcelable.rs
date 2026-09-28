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

//! Parcelable trait and utilities for serializable types.
//!
//! This module defines the core traits and utilities for types that can be
//! serialized and deserialized in binder parcels, providing the foundation
//! for AIDL-generated types and custom parcelable implementations.
//!
//! # Receiving `BINDER_TYPE_BINDER`
//!
//! Cross-process binder transfers reach the receiver as `BINDER_TYPE_HANDLE`;
//! the kernel emits `BINDER_TYPE_BINDER` only when it routes a binder back to
//! its publisher, this process. The id is therefore looked up in the process's
//! native-binder table. An unknown id means either a kernel bug surfacing a
//! binder never published here, or an entry already torn down, which the round
//! trip rules out because the receiving process's outstanding handle keeps
//! `kernel_refs > 0`. Either way it is an integrity error, reported as
//! `DeadObject`.

use crate::{binder::*, error::*, parcel::Parcel, process_state::*, sys::*};

/// Core trait for types that can be serialized to and from parcels.
///
/// This trait is equivalent to `android::Parcelable` in C++, and defines
/// the basic interface that all parcelable types must implement for binder IPC.
/// It provides low-level serialization methods that work directly with parcel data.
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
        Stability::default()
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
                parcel.write_le(self)
            }
        }
    };

    {Deserialize, $ty:ty} => {
        impl Deserialize for $ty {
            fn deserialize(parcel: &mut Parcel) -> Result<Self> {
                parcel.read_le::<$ty>()
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
                // Widen first: the byte order applies to the 4-byte `$to_ty` slot, not to `$ty`.
                let val: $to_ty = *self as _;
                parcel.write_le(&val)
            }
        }
    };

    {Deserialize, $to_ty:ty, $ty:ty} => {
        impl Deserialize for $ty {
            fn deserialize(parcel: &mut Parcel) -> Result<Self> {
                Ok(parcel.read_le::<$to_ty>()? as _)
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
        Ok(parcel.read_le::<i32>()? != 0)
    }
}

impl DeserializeArray for bool {}

impl Serialize for bool {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        let val: i32 = *self as _;
        parcel.write_le(&val)
    }
}

impl SerializeArray for bool {}

impl SerializeOption for str {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        match this {
            None => parcel.write::<i32>(&-1),

            Some(text) => {
                let mut utf16 = Vec::with_capacity(text.len() + 2); // Room for NUL and padding.
                utf16.extend(text.encode_utf16());

                let len = utf16.len();

                utf16.push(0);

                parcel.write::<i32>(&(len as i32))?;

                // Pre-swap units to LE so the byte view below is the wire form; LE builds omit it.
                if cfg!(target_endian = "big") {
                    for unit in utf16.iter_mut() {
                        *unit = unit.swap_bytes();
                    }
                }

                // SAFETY: the view covers exactly `utf16`'s bytes and ends with this call.
                parcel.write_aligned_data(unsafe {
                    std::slice::from_raw_parts(
                        utf16.as_ptr() as *const u8,
                        utf16.len() * std::mem::size_of::<u16>(),
                    )
                })?;

                Ok(())
            }
        }
    }
}

impl Deserialize for StatusCode {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Ok(parcel.read_le::<i32>()?.into())
    }
}

impl Serialize for StatusCode {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        let val: i32 = i32::from(*self);
        parcel.write_le(&val)
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
                parcel.write_aligned(self)
            }
        }
    };

    {Deserialize, $ty:ty} => {
        impl Deserialize for $ty {
            fn deserialize(parcel: &mut Parcel) -> Result<Self> {
                const SIZE: usize = std::mem::size_of::<$ty>();
                // SAFETY: `$ty` is a POD binder-ABI struct (see below): any `[u8; SIZE]` is valid.
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

        if len == -1 {
            return Ok(None);
        }

        if (0..i32::MAX).contains(&len) {
            // `len + 1` units incl. NUL, checked so a hostile `len` cannot wrap on 32-bit targets.
            let (byte_count, _) =
                crate::parcel::checked_array_layout(len + 1, std::mem::size_of::<u16>())?;
            let data = parcel.read_aligned_data(byte_count)?;
            // A `&[u16]` view of the 1-byte-aligned buffer would be UB, so copy into a `Vec<u16>`.
            let u16_data: Vec<u16> = data[..len as usize * std::mem::size_of::<u16>()]
                .chunks_exact(std::mem::size_of::<u16>())
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let res = String::from_utf16(&u16_data).map_err(|e| {
                log::error!("Deserialize for Option<String16>: {e}");
                StatusCode::BadValue
            })?;

            return Ok(Some(res));
        }

        Err(StatusCode::UnexpectedNull)
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
        parcel.read_object(false)
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
        // No hooks = data-only mode: refuse before `binder.into()`, which panics without a driver.
        if !parcel.is_kernel_backed() {
            #[cfg(feature = "rpc")]
            if let Some(ops) = parcel.rpc_ops() {
                return ops.write_binder(this, parcel);
            }
            return Err(StatusCode::BadType);
        }

        match this {
            Some(binder) => {
                // Reject RPC binders (AOSP) before `into()`, which panics with no ProcessState.
                #[cfg(feature = "rpc")]
                if (**binder)
                    .as_any()
                    .downcast_ref::<crate::rpc::RpcProxy>()
                    .is_some()
                {
                    log::error!("Sending a socket (RPC) binder over kernel binder is prohibited");
                    return Err(StatusCode::InvalidOperation);
                }
                parcel.write_binder_object(&binder.into(), binder)?;
                if crate::sdk_at_least(30) {
                    parcel.write::<i32>(&binder.stability().into())?;
                }
                // Freeze stability once parceled (AOSP `BBinder::setParceled`); no-op for proxies.
                binder.set_parceled();
                Ok(())
            }

            None => {
                parcel.write::<flat_binder_object>(&flat_binder_object::default())?;
                if crate::sdk_at_least(30) {
                    parcel.write::<i32>(&Stability::Local.into())?;
                }

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
        // No hooks = data-only mode: no table could turn the bytes into a binder.
        if !parcel.is_kernel_backed() {
            #[cfg(feature = "rpc")]
            if let Some(ops) = parcel.rpc_ops() {
                return ops.read_binder(parcel);
            }
            return Err(StatusCode::BadType);
        }

        let flat: flat_binder_object = parcel.read()?;
        let stability: i32 = if crate::sdk_at_least(30) {
            parcel.read()?
        } else {
            Stability::Local.into()
        };

        match flat.header_type() {
            BINDER_TYPE_BINDER => {
                // Only the publisher receives BINDER_TYPE_BINDER; see module doc for an unknown id.
                let id = flat.pointer();
                if id != 0 {
                    let arc = ProcessState::as_self().lookup_native(id).ok_or_else(|| {
                        log::error!("BINDER_TYPE_BINDER for unknown native id {id}");
                        StatusCode::DeadObject
                    })?;
                    Ok(Some(SIBinder::from_arc(arc)))
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
                log::warn!(
                    "Unknown Binder Type ({}) was delivered.",
                    flat.header_type()
                );
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
///
/// It exists instead of `Serialize for Option<T>` because the orphan rule
/// forbids `impl Serialize for Option<&dyn IFoo>` for AIDL interfaces, while
/// `impl SerializeOption for dyn IFoo` is allowed. It also carries the default
/// implementation for AIDL-generated parcelables.
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
        match null {
            NULL_PARCELABLE_FLAG => Ok(None),
            NON_NULL_PARCELABLE_FLAG => parcel.read().map(Some),
            _ => Err(StatusCode::UnexpectedNull),
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
///
/// It is a separate trait because, without stable specialization, that is the
/// only way to give most types a default method while a few (`u8`'s
/// `writeByteArray`) override it.
pub trait SerializeArray: Serialize + Sized {
    /// Serialize an array of this type into the given parcel.
    fn serialize_array(slice: &[Self], parcel: &mut Parcel) -> Result<()> {
        // An `i32` length word: an oversized slice is `BadValue`, as in AOSP's Rust binder.
        let len: i32 = slice.len().try_into().or(Err(StatusCode::BadValue))?;
        parcel.write::<i32>(&len)?;

        for s in slice {
            parcel.write(s)?;
        }

        Ok(())
    }
}

/// Helper trait for types that can be deserialized as arrays.
/// Defaults to calling Deserialize::deserialize() manually for every element,
/// but can be overridden for custom implementations like `readByteArray`.
///
/// The default implementation caps its speculative pre-allocation at
/// `data_avail() / 4` elements. Every element on that path costs at least 4
/// wire bytes (`read_aligned_data` pads to 4; the byte-sized primitives
/// override `deserialize_array`), so a well-formed array always satisfies
/// `len <= data_avail() / 4`. The cap divides the wire bytes rather than
/// clamping the count because `Vec::with_capacity` multiplies by
/// `size_of::<Self>()`: under a count-only clamp, a 64 MiB RPC frame declaring
/// `len = 64_000_000` for `Vec<String>` (24 B each) requests about 1.5 GiB up
/// front, and an allocation failure aborts the process. Valid input decodes
/// identically; the loop still pushes exactly `len`, growing on demand.
pub trait DeserializeArray: Deserialize {
    /// Deserialize an array of type from the given parcel.
    fn deserialize_array(parcel: &mut Parcel) -> Result<Option<Vec<Self>>> {
        let len: i32 = parcel.read()?;
        if len < -1 {
            log::error!("Negative array size given in parcel: {len}");
            return Err(StatusCode::UnexpectedNull);
        }
        if len == -1 {
            return Ok(None);
        }
        if len == 0 {
            return Ok(Some(Vec::new()));
        }
        // Pre-allocate at most one element per 4 wire bytes left; see the trait rustdoc.
        let cap = (len as usize).min(parcel.data_avail() / 4);
        let mut res: Vec<Self> = Vec::with_capacity(cap);

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
        DeserializeArray::deserialize_array(parcel)?.ok_or(StatusCode::UnexpectedNull)
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
        vec.try_into().map_err(|_| {
            log::error!("Deserialize: Failed to convert Vec<T> to [T; N]");
            StatusCode::BadValue
        })
    }
}

impl<T: std::fmt::Debug + DeserializeArray, const N: usize> DeserializeOption for [T; N] {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        let vec = DeserializeArray::deserialize_array(parcel)?;
        vec.map(|v| {
            v.try_into().map_err(|_| {
                log::error!("DeserializeOption: Failed to convert Vec<T> to [T; N]");
                StatusCode::BadValue
            })
        })
        .transpose()
    }
}

impl<T: DeserializeArray, const N: usize> DeserializeArray for [T; N] {}

#[cfg(test)]
mod tests {
    //! # Notes
    //!
    //! - `hostile_string16_length_returns_err_not_panic`: on 32-bit targets the `(len + 1) * 2`
    //!   byte count — and the `+ 3` inside `pad_size` — could wrap `usize` and slip past the
    //!   bounds check into a slice-index panic; routing through `checked_array_layout` rejects
    //!   it as `BadValue`. 64-bit surfaces it as `NotEnoughData`; either way it is an `Err`.
    //! - `generic_vec_deserialize_rejects_null_but_accepts_empty`: the generic
    //!   `DeserializeArray::deserialize_array` default is the path of `String` / `bool` /
    //!   parcelable elements, distinct from the `u8` / `u16` fast paths in `parcel.rs`. A
    //!   non-null `Vec<T>` rejects null; an `Option<Vec<T>>` maps `0` to `Some(empty)`, not
    //!   `None`.
    //! - `default_option_rejects_garbage_sentinel`: the default
    //!   `DeserializeOption::deserialize_option` serves primitive `Option<T>` such as
    //!   `Option<i32>`; any sentinel other than `0` / `1` is `UnexpectedNull`, matching AOSP's
    //!   `readData(Parcelable*)` `present != kNonNullParcelableFlag`.

    use super::*;

    /// A hostile String16 length is an `Err`, not a panic; see this module's doc.
    #[test]
    fn hostile_string16_length_returns_err_not_panic() {
        for len in [-2, i32::MAX - 1, i32::MAX] {
            let mut p = Parcel::new();
            p.write::<i32>(&len).unwrap();
            p.set_data_position(0);
            let r = <String as DeserializeOption>::deserialize_option(&mut p);
            assert!(r.is_err(), "len {len} returned {r:?}");
        }
    }

    /// A normal string still round-trips byte-identically.
    #[test]
    fn string16_round_trip() {
        let mut p = Parcel::new();
        let s = "héllo".to_string();
        p.write(&s).unwrap();
        p.set_data_position(0);
        let got: String = p.read::<String>().unwrap();
        assert_eq!(got, s);
    }

    /// An empty string (len 0, NUL only) round-trips past the `len + 1` layout check.
    #[test]
    fn empty_string16_round_trip() {
        let mut p = Parcel::new();
        let s = String::new();
        p.write(&s).unwrap();
        p.set_data_position(0);
        let got: String = p.read::<String>().unwrap();
        assert_eq!(got, s);
    }

    /// Generic `deserialize_array` default: -1 is null, 0 is empty, other negatives are rejected.
    #[test]
    fn generic_vec_deserialize_rejects_null_but_accepts_empty() {
        let mut p = Parcel::new();
        p.write(&-1i32).unwrap(); // null
        p.write(&-1i32).unwrap(); // null
        p.write(&0i32).unwrap(); // empty
        p.write(&0i32).unwrap(); // empty
        p.write(&-2i32).unwrap(); // malformed
        p.set_data_position(0);

        assert_eq!(p.read::<Vec<String>>(), Err(StatusCode::UnexpectedNull));
        assert_eq!(p.read::<Option<Vec<String>>>(), Ok(None));
        assert_eq!(p.read::<Vec<String>>(), Ok(Vec::new()));
        assert_eq!(p.read::<Option<Vec<String>>>(), Ok(Some(Vec::new())));
        assert_eq!(p.read::<Vec<String>>(), Err(StatusCode::UnexpectedNull));
    }

    /// Default `deserialize_option` takes only 0/1, as AOSP `present != kNonNullParcelableFlag`.
    #[test]
    fn default_option_rejects_garbage_sentinel() {
        let mut p = Parcel::new();
        p.write(&NULL_PARCELABLE_FLAG).unwrap(); // 0 → None
        p.write(&NON_NULL_PARCELABLE_FLAG).unwrap(); // 1 → present
        p.write(&42i32).unwrap(); //     payload
        p.write(&2i32).unwrap(); // garbage sentinel
        p.set_data_position(0);

        assert_eq!(p.read::<Option<i32>>(), Ok(None));
        assert_eq!(p.read::<Option<i32>>(), Ok(Some(42)));
        assert_eq!(p.read::<Option<i32>>(), Err(StatusCode::UnexpectedNull));
    }

    /// `impl_deserialize_for_parcelable!` (the AIDL parcelable path) rejects sentinels beyond 0/1.
    #[test]
    fn macro_parcelable_rejects_garbage_sentinel() {
        #[derive(Default, Debug, PartialEq)]
        struct Tiny {
            x: i32,
        }
        impl Parcelable for Tiny {
            fn write_to_parcel(&self, parcel: &mut Parcel) -> Result<()> {
                parcel.write(&self.x)
            }
            fn read_from_parcel(&mut self, parcel: &mut Parcel) -> Result<()> {
                self.x = parcel.read()?;
                Ok(())
            }
        }
        crate::impl_deserialize_for_parcelable!(Tiny);

        let mut p = Parcel::new();
        p.write(&NON_NULL_PARCELABLE_FLAG).unwrap(); // 1 → present
        p.write(&7i32).unwrap(); //     payload
        p.write(&NULL_PARCELABLE_FLAG).unwrap(); // 0 → None
        p.write(&2i32).unwrap(); // garbage sentinel
        p.set_data_position(0);

        assert_eq!(p.read::<Tiny>(), Ok(Tiny { x: 7 }));
        assert_eq!(p.read::<Option<Tiny>>(), Ok(None));
        // Any non-{0,1} flag is UNEXPECTED_NULL, as in AOSP `Parcel::readData`.
        assert_eq!(p.read::<Option<Tiny>>(), Err(StatusCode::UnexpectedNull));
    }
}
