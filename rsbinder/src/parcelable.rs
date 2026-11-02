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

use crate::{
    binder::*, binder_object::FlatBinderObject, error::*, parcel::Parcel, process_state::*, sys::*,
};

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
                // The UTF-8 length bounds the UTF-16 unit count, so a shorter text fits the stack.
                const STACK_UNITS: usize = 128;
                let mut stack = [0u8; 2 * STACK_UNITS];
                let mut heap;
                // Zeroed, room for every unit plus the NUL.
                let buf: &mut [u8] = if text.len() < STACK_UNITS {
                    &mut stack[..2 * (text.len() + 1)]
                } else {
                    heap = vec![0u8; 2 * (text.len() + 1)];
                    &mut heap
                };
                let mut units = 0;
                for (unit, dst) in text.encode_utf16().zip(buf.chunks_exact_mut(2)) {
                    dst.copy_from_slice(&unit.to_le_bytes());
                    units += 1;
                }

                parcel.write::<i32>(&(units as i32))?;
                // The units and the NUL unit after them, still zero.
                parcel.write_aligned_data(&buf[..2 * (units + 1)])
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

/// A `String16`'s unit bytes, NUL excluded; null or malformed input is `None`, as in AOSP NDK.
pub(crate) fn read_string16_units(parcel: &mut Parcel) -> Option<&[u8]> {
    let len = parcel.read::<i32>().ok()?;
    if !(0..i32::MAX).contains(&len) {
        return None;
    }

    // `len + 1` units incl. NUL, checked so a hostile `len` cannot wrap on 32-bit targets.
    let (byte_count, _) =
        crate::parcel::checked_array_layout(len + 1, std::mem::size_of::<u16>()).ok()?;
    // Bounds-checked against the parcel before anything is allocated for it.
    let data = parcel.read_aligned_data(byte_count).ok()?;
    let end = byte_count - std::mem::size_of::<u16>();
    (data[end..] == [0, 0]).then_some(&data[..end])
}

/// The units of `String16` bytes; a `&[u16]` view of the 1-byte-aligned buffer would be UB.
fn string16_units(bytes: &[u8]) -> impl Iterator<Item = u16> + '_ {
    bytes
        .chunks_exact(std::mem::size_of::<u16>())
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
}

/// Decodes `String16` unit bytes, skipping unpaired surrogates as AOSP does.
fn decode_string16_units(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() / std::mem::size_of::<u16>());
    for c in char::decode_utf16(string16_units(bytes)).flatten() {
        text.push(c);
    }
    text
}

/// Compare raw interface-token units in place; decode only the mismatch diagnostic.
pub(crate) fn read_string16_matches(
    parcel: &mut Parcel,
    expected: &str,
) -> Result<std::result::Result<(), String>> {
    let units = read_string16_units(parcel).ok_or(StatusCode::UnexpectedNull)?;
    // Unequal lengths are unequal: `Iterator::eq` checks both ends.
    if string16_units(units).eq(expected.encode_utf16()) {
        Ok(Ok(()))
    } else {
        Ok(Err(decode_string16_units(units)))
    }
}

impl DeserializeOption for String {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        Ok(read_string16_units(parcel).map(decode_string16_units))
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

impl Deserialize for FlatBinderObject {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        parcel.read_object(false)
    }
}

impl Serialize for FlatBinderObject {
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

/// The kernel parcel's stability word on a host of SDK `sdk`: `Stability` rustdoc "Wire encoding".
fn kernel_stability_word(stability: Stability, sdk: u32) -> i32 {
    if sdk == 31 || sdk == 32 {
        crate::binder::android12_category_repr(stability.level())
    } else {
        stability.level()
    }
}

/// The SDK `kernel_stability_word` is given: the device's on Android, 0 off it.
fn host_sdk() -> u32 {
    #[cfg(target_os = "android")]
    {
        crate::get_android_sdk_version()
    }
    #[cfg(not(target_os = "android"))]
    {
        0
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
                    parcel.write(&kernel_stability_word(binder.stability(), host_sdk()))?;
                }
                // Freeze stability once parceled (AOSP `BBinder::setParceled`); no-op for proxies.
                binder.set_parceled();
                Ok(())
            }

            None => {
                parcel.write::<FlatBinderObject>(&FlatBinderObject::default())?;
                if crate::sdk_at_least(30) {
                    parcel.write(&kernel_stability_word(Stability::Local, host_sdk()))?;
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

        let flat: FlatBinderObject = parcel.read()?;
        let stability: Option<i32> = if crate::sdk_at_least(30) {
            Some(parcel.read()?)
        } else {
            None
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
                // Decoded before the lookup, so a level naming no `Stability` creates no proxy.
                let level = stability
                    .map(Stability::try_from)
                    .transpose()?
                    .map(Stability::level);
                let proxy = ProcessState::as_self().proxy_for_handle(flat.handle())?;
                // The kernel path's one fill: a cached proxy (e.g. pinned by `append_from`) too.
                if let Some(level) = level {
                    proxy.record_received_stability(level);
                }
                Ok(Some(SIBinder::from_arc(proxy)))
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
/// identically; each element read requires at least one complete wire word.
pub trait DeserializeArray: Deserialize {
    /// Deserialize an array of type from the given parcel.
    fn deserialize_array(parcel: &mut Parcel) -> Result<Option<Vec<Self>>> {
        let len: i32 = parcel.read()?;
        if len < -1 {
            log::error!("Negative array size given in parcel: {len}");
            return Err(StatusCode::BadValue);
        }
        if len == -1 {
            return Ok(None);
        }
        if len == 0 {
            return Ok(Some(Vec::new()));
        }
        if len as usize > parcel.data_avail() {
            return Err(StatusCode::NoMemory);
        }
        // Pre-allocate at most one element per 4 wire bytes left; see the trait rustdoc.
        let cap = (len as usize).min(parcel.data_avail() / 4);
        let mut res: Vec<Self> = Vec::with_capacity(cap);

        for _ in 0..len {
            if parcel.data_avail() < 4 {
                return Err(StatusCode::NotEnoughData);
            }
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
    //! - `hostile_string16_length_returns_err_not_panic`: malformed String16 is null in AOSP;
    //!   a non-nullable String still returns `UnexpectedNull`, without overflow or allocation.
    //! - `generic_vec_deserialize_rejects_null_but_accepts_empty`: the generic
    //!   `DeserializeArray::deserialize_array` default is the path of `String` / `bool` /
    //!   parcelable elements, distinct from the `u8` / `u16` fast paths in `parcel.rs`. A
    //!   non-null `Vec<T>` rejects null; an `Option<Vec<T>>` maps `0` to `Some(empty)`, not
    //!   `None`.
    //! - `default_option_accepts_nonzero_presence`: the default
    //!   `DeserializeOption::deserialize_option` serves primitive `Option<T>` such as
    //!   `Option<i32>`; only zero is null, matching AOSP Rust's default implementation.

    use super::*;

    /// The kernel word is android-12's `Category` on SDK 31/32 alone, the raw level elsewhere.
    #[test]
    fn kernel_stability_word_is_the_category_on_sdk_31_and_32_only() {
        for (s, level) in [
            (Stability::Local, 0),
            (Stability::Vendor, 0b00_0011),
            (Stability::System, 0b00_1100),
            (Stability::Vintf, 0b11_1111),
        ] {
            for sdk in [0, 29, 30, 33, 34, 36] {
                assert_eq!(kernel_stability_word(s, sdk), level, "{s:?} on SDK {sdk}");
            }
            for sdk in [31, 32] {
                let word = kernel_stability_word(s, sdk);
                assert_eq!(word, (level << 24) | 1, "{s:?} on SDK {sdk}");
                assert_eq!(Stability::try_from(word), Ok(s), "{s:?} on SDK {sdk}");
            }
        }
    }

    /// A hostile String16 length is an `Err`, not a panic; see this module's doc.
    #[test]
    fn hostile_string16_length_returns_err_not_panic() {
        for len in [-2, 0, 1, i32::MAX - 1, i32::MAX] {
            let mut p = Parcel::new();
            p.write::<i32>(&len).unwrap();
            p.set_data_position(0);
            assert_eq!(p.read::<Option<String>>(), Ok(None), "len {len}");
            p.set_data_position(0);
            assert_eq!(p.read::<String>(), Err(StatusCode::UnexpectedNull));
        }
        let mut p = Parcel::new();
        assert_eq!(p.read::<Option<String>>(), Ok(None));
        assert_eq!(p.read::<String>(), Err(StatusCode::UnexpectedNull));
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

    /// `len`, the LE units, a NUL unit, zero padding to 4: built without the encoder under test.
    fn string16_wire(s: &str) -> Vec<u8> {
        let units: Vec<u16> = s.encode_utf16().collect();
        let mut wire = (units.len() as i32).to_le_bytes().to_vec();
        for unit in units.iter().chain(&[0u16]) {
            wire.extend_from_slice(&unit.to_le_bytes());
        }
        while wire.len() % 4 != 0 {
            wire.push(0);
        }
        wire
    }

    fn string16_samples() -> Vec<String> {
        let mut samples = Vec::new();
        // Both sides of the encoder's stack/heap split, with 1-, 3- and 4-byte UTF-8 (BMP, pairs).
        for n in [0usize, 1, 2, 42, 126, 127, 128, 129, 300] {
            samples.push("a".repeat(n));
            samples.push("\u{20AC}".repeat(n));
            samples.push("\u{1F600}".repeat(n));
            samples.push(format!("{}\u{1F600}", "b".repeat(n)));
        }
        samples
    }

    #[test]
    fn string16_wire_is_the_unit_encoding_across_the_stack_bound() {
        for s in string16_samples() {
            let mut p = Parcel::new();
            p.write(&s).unwrap();
            p.write(&7i32).unwrap();
            let mut want = string16_wire(&s);
            want.extend_from_slice(&7i32.to_le_bytes());
            assert_eq!(p.as_bytes().unwrap(), &want[..], "{} UTF-8 bytes", s.len());

            p.set_data_position(0);
            assert_eq!(p.read::<String>().unwrap(), s);
            assert_eq!(
                p.read::<i32>().unwrap(),
                7,
                "cursor after {} UTF-8 bytes",
                s.len()
            );
        }
    }

    #[test]
    fn string16_compare_matches_a_read_and_leaves_the_same_cursor() {
        for s in string16_samples() {
            let mut p = Parcel::new();
            p.write(&s).unwrap();
            p.write(&7i32).unwrap();

            p.set_data_position(0);
            assert_eq!(read_string16_matches(&mut p, &s), Ok(Ok(())));
            assert_eq!(p.read::<i32>().unwrap(), 7);

            // Longer, shorter and same-length-different expectations all report what was read.
            let longer = format!("{s}x");
            let mut shorter = s.clone();
            let mut same_len = s.clone();
            let mut others = vec![longer];
            if shorter.pop().is_some() {
                others.push(shorter);
                same_len.pop();
                same_len.push('z');
                others.push(same_len);
            }
            for other in others {
                p.set_data_position(0);
                assert_eq!(read_string16_matches(&mut p, &other), Ok(Err(s.clone())));
                assert_eq!(p.read::<i32>().unwrap(), 7);
            }
        }
    }

    #[test]
    fn string16_compare_rejects_null_and_keeps_raw_units() {
        let mut p = Parcel::new();
        p.write(&-1i32).unwrap();
        p.set_data_position(0);
        assert_eq!(
            read_string16_matches(&mut p, ""),
            Err(StatusCode::UnexpectedNull)
        );

        // String conversion skips bad surrogates; interface tokens must still compare raw units.
        for (len, units, expected, decoded) in [
            (1, 0xD800u32, "x", ""),
            (1, 0xD800, "", ""),
            (1, 0xDC00, "", ""),
            (2, 0x0041_D800, "A", "A"),
        ] {
            let mut p = Parcel::new();
            p.write(&len).unwrap();
            p.write(&units).unwrap();
            if len == 2 {
                p.write(&0i32).unwrap();
            }
            p.write(&7i32).unwrap();
            p.set_data_position(0);
            assert_eq!(
                read_string16_matches(&mut p, expected),
                Ok(Err(decoded.to_owned()))
            );
            assert_eq!(p.read::<i32>(), Ok(7));
            p.set_data_position(0);
            assert_eq!(p.read::<String>(), Ok(decoded.to_owned()));
            assert_eq!(p.read::<i32>(), Ok(7));
        }

        let mut p = Parcel::new();
        p.write(&1i32).unwrap();
        p.write(&0x0042_0041u32).unwrap(); // 'A' followed by a non-NUL terminator.
        p.write(&7i32).unwrap();
        p.set_data_position(0);
        assert_eq!(p.read::<Option<String>>(), Ok(None));
        assert_eq!(p.read::<i32>(), Ok(7));
        p.set_data_position(0);
        assert_eq!(p.read::<String>(), Err(StatusCode::UnexpectedNull));
        p.set_data_position(0);
        assert_eq!(
            read_string16_matches(&mut p, "A"),
            Err(StatusCode::UnexpectedNull)
        );
        assert_eq!(p.read::<i32>(), Ok(7));

        for len in [-2, i32::MAX - 1, i32::MAX] {
            let mut p = Parcel::new();
            p.write::<i32>(&len).unwrap();
            p.set_data_position(0);
            assert!(read_string16_matches(&mut p, "x").is_err(), "len {len}");
        }
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
        assert_eq!(p.read::<Vec<String>>(), Err(StatusCode::BadValue));

        let mut p = Parcel::new();
        p.write(&1i32).unwrap();
        p.set_data_position(0);
        assert_eq!(p.read::<Vec<bool>>(), Err(StatusCode::NoMemory));
        p.set_data_position(0);
        p.write(&2i32).unwrap();
        p.write(&1i32).unwrap();
        p.set_data_position(0);
        assert_eq!(p.read::<Vec<bool>>(), Err(StatusCode::NotEnoughData));

        p.set_data_position(0);
        p.write(&4i32).unwrap();
        p.write(&-1i32).unwrap();
        p.set_data_position(0);
        assert_eq!(
            p.read::<Vec<Option<String>>>(),
            Err(StatusCode::NotEnoughData)
        );
    }

    /// The default nullable codec treats every nonzero presence as present, as AOSP Rust does.
    #[test]
    fn default_option_accepts_nonzero_presence() {
        for presence in [0, 1, 2, -1, i32::MAX, i32::MIN] {
            let mut p = Parcel::new();
            p.write(&presence).unwrap();
            if presence != 0 {
                p.write(&42i32).unwrap();
            }
            p.write(&7i32).unwrap();
            p.set_data_position(0);
            let expected = if presence == 0 { None } else { Some(42) };
            assert_eq!(p.read::<Option<i32>>(), Ok(expected));
            assert_eq!(p.read::<i32>(), Ok(7));
            p.set_data_position(0);
            let mut value = Some(99);
            p.read_onto(&mut value).unwrap();
            assert_eq!(value, expected);
            assert_eq!(p.read::<i32>(), Ok(7));
        }
    }

    /// Structured parcelables treat only zero as null, as AOSP Rust does.
    #[test]
    fn macro_parcelable_accepts_nonzero_presence() {
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

        let sentinel = 0x1234_5678i32;
        for presence in [0, 1, 2, -1, i32::MAX, i32::MIN] {
            let mut p = Parcel::new();
            p.write(&presence).unwrap();
            if presence != 0 {
                Tiny { x: 7 }.write_to_parcel(&mut p).unwrap();
            }
            p.write(&sentinel).unwrap();

            p.set_data_position(0);
            assert_eq!(
                Tiny::deserialize(&mut p),
                if presence == 0 {
                    Err(StatusCode::UnexpectedNull)
                } else {
                    Ok(Tiny { x: 7 })
                },
                "presence {presence}"
            );
            assert_eq!(p.read::<i32>(), Ok(sentinel));

            p.set_data_position(0);
            let mut value = Tiny { x: 99 };
            assert_eq!(
                value.deserialize_from(&mut p),
                if presence == 0 {
                    Err(StatusCode::UnexpectedNull)
                } else {
                    Ok(())
                },
                "presence {presence}"
            );
            assert_eq!(value.x, if presence == 0 { 99 } else { 7 });
            assert_eq!(p.read::<i32>(), Ok(sentinel));

            p.set_data_position(0);
            assert_eq!(
                Tiny::deserialize_option(&mut p),
                Ok(if presence == 0 {
                    None
                } else {
                    Some(Tiny { x: 7 })
                }),
                "presence {presence}"
            );
            assert_eq!(p.read::<i32>(), Ok(sentinel));

            for mut value in [None, Some(Tiny { x: 99 })] {
                p.set_data_position(0);
                assert_eq!(
                    Tiny::deserialize_option_from(&mut value, &mut p),
                    Ok(()),
                    "presence {presence}"
                );
                assert_eq!(
                    value,
                    if presence == 0 {
                        None
                    } else {
                        Some(Tiny { x: 7 })
                    }
                );
                assert_eq!(p.read::<i32>(), Ok(sentinel));
            }
        }

        for presence in [
            None,
            Some(1),
            Some(2),
            Some(-1),
            Some(i32::MAX),
            Some(i32::MIN),
        ] {
            let mut p = Parcel::new();
            if let Some(presence) = presence {
                p.write(&presence).unwrap();
            }
            // Tiny requires its field; a sentinel here would make the body valid.
            p.set_data_position(0);
            assert_eq!(
                Tiny::deserialize(&mut p),
                Err(StatusCode::NotEnoughData),
                "presence {presence:?}"
            );

            p.set_data_position(0);
            let mut value = Tiny { x: 99 };
            assert_eq!(
                value.deserialize_from(&mut p),
                Err(StatusCode::NotEnoughData),
                "presence {presence:?}"
            );

            p.set_data_position(0);
            assert_eq!(
                Tiny::deserialize_option(&mut p),
                Err(StatusCode::NotEnoughData),
                "presence {presence:?}"
            );

            for mut value in [None, Some(Tiny { x: 99 })] {
                p.set_data_position(0);
                assert_eq!(
                    Tiny::deserialize_option_from(&mut value, &mut p),
                    Err(StatusCode::NotEnoughData),
                    "presence {presence:?}"
                );
            }
        }
    }

    /// A kernel parcel holding `handle` followed by its stability `word`; writing it pins a proxy.
    fn kernel_handle_parcel(handle: u32, word: i32) -> Parcel {
        let mut p = Parcel::new();
        p.write(&FlatBinderObject::new_handle(handle, 0))
            .expect("handle");
        p.write(&word).expect("stability");
        p.set_data_position(0);
        p
    }

    /// The stability word `binder` is written with into a kernel parcel.
    fn kernel_word_sent_for(binder: &SIBinder) -> i32 {
        let mut p = Parcel::new();
        p.write(binder).expect("write");
        p.set_data_position(FlatBinderObject::SIZE);
        p.read::<i32>().expect("stability")
    }

    /// Whether a kernel stability-word test can run: a binder driver opens and the word exists.
    fn kernel_stability_word_testable() -> bool {
        if !ProcessState::is_initialized() && ProcessState::init_default().is_err() {
            eprintln!("skipped: no kernel binder driver");
            return false;
        }
        crate::sdk_at_least(30)
    }

    // The handles below name no node: the driver logs and skips their refcounts, which is enough.

    /// A proxy revived from a `WIBinder` (cache case (b)) keeps the level its handle received.
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn a_revived_kernel_proxy_keeps_its_received_level() {
        if !kernel_stability_word_testable() {
            return;
        }
        let handle = 0x5AB0_0004;
        let word = kernel_stability_word(Stability::Vintf, host_sdk());
        let received: SIBinder = kernel_handle_parcel(handle, word).read().expect("vintf");
        let weak = SIBinder::downgrade(&received);
        drop(received);
        let revived = ProcessState::as_self()
            .strong_proxy_for_handle(handle)
            .expect("revive");
        assert!(weak == revived, "the same (handle, generation): case (b)");
        assert_eq!(revived.stability(), Stability::Vintf, "getter");
        assert_eq!(kernel_word_sent_for(&revived), word, "sent back");
    }

    /// A proxy pinned before its word is read (a `ParcelableHolder` payload) takes its level.
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn a_relayed_kernel_proxy_reports_and_sends_back_its_level() {
        if !kernel_stability_word_testable() {
            return;
        }
        for (handle, stability) in [
            (0x5AB0_0001, Stability::Vendor),
            (0x5AB0_0002, Stability::Vintf),
        ] {
            let word = kernel_stability_word(stability, host_sdk());
            let mut source = kernel_handle_parcel(handle, word);
            // `ParcelableHolder::read_from_parcel`'s cut: `append_from` pins the handle again.
            let mut payload = source
                .sub_parcel(0, source.data_size())
                .expect("sub-parcel");
            payload.set_data_position(0);
            let proxy: SIBinder = payload.read().expect("proxy");
            assert_eq!(proxy.stability(), stability, "{stability:?} getter");
            assert_eq!(
                kernel_word_sent_for(&proxy),
                word,
                "{stability:?} sent back"
            );
            // A fresh source pins through the public lookup, then is read: the same answer.
            let again: SIBinder = kernel_handle_parcel(handle, word).read().expect("again");
            assert_eq!(again.stability(), stability, "{stability:?} read again");
        }
    }

    /// An `UNDECLARED` word leaves a kernel proxy unset (AOSP `mStability` = 0); the first sticks.
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn a_kernel_proxy_is_filled_by_its_first_declared_word() {
        if !kernel_stability_word_testable() {
            return;
        }
        let handle = 0x5AB0_0003;
        let word = |s: Stability| kernel_stability_word(s, host_sdk());

        let first: SIBinder = kernel_handle_parcel(handle, word(Stability::Local))
            .read()
            .expect("undeclared");
        assert_eq!(
            first.stability(),
            Stability::System,
            "unset: the local level"
        );
        assert_eq!(kernel_word_sent_for(&first), word(Stability::System));

        let filled: SIBinder = kernel_handle_parcel(handle, word(Stability::Vintf))
            .read()
            .expect("vintf");
        assert!(
            std::sync::Arc::ptr_eq(first.as_arc(), filled.as_arc()),
            "one cached proxy"
        );
        assert_eq!(
            first.stability(),
            Stability::Vintf,
            "the first declared word fills it"
        );

        let conflicting: SIBinder = kernel_handle_parcel(handle, word(Stability::Vendor))
            .read()
            .expect("vendor");
        assert_eq!(
            conflicting.stability(),
            Stability::Vintf,
            "the first one stays"
        );
        assert_eq!(kernel_word_sent_for(&first), word(Stability::Vintf));
    }

    /// A binder built with `Stability::Local` goes out with this process's level.
    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    #[serial_test::serial(binder)]
    fn a_local_binder_is_sent_with_the_local_level_on_kernel_binder() {
        struct Svc;
        impl crate::Interface for Svc {}
        impl crate::Remotable for Svc {
            fn descriptor() -> &'static str {
                "rsbinder.test.parcelable.ILocal"
            }
            fn on_transact(
                &self,
                _code: crate::TransactionCode,
                _reader: &mut Parcel,
                _reply: &mut Parcel,
            ) -> Result<()> {
                Err(StatusCode::UnknownTransaction)
            }
            fn on_dump(&self, _w: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
                Ok(())
            }
        }
        let local =
            crate::Interface::as_binder(&crate::Binder::new_with_stability(Svc, Stability::Local));
        // The level `serialize_option` writes; no driver needed to read it.
        assert_eq!(local.stability(), Stability::System);
        if !kernel_stability_word_testable() {
            return;
        }
        assert_eq!(
            kernel_word_sent_for(&local),
            kernel_stability_word(Stability::System, host_sdk())
        );
    }
}
