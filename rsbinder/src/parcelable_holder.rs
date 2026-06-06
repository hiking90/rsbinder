// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

/*
 * Copyright (C) 2021 The Android Open Source Project
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

//! Generic container for parcelable objects.
//!
//! This module provides `ParcelableHolder`, a type-erased container that can hold
//! any parcelable object. It's primarily used for AIDL union types and other
//! scenarios where the specific parcelable type is not known at compile time.

use crate::binder::Stability;
use crate::error::{Result, StatusCode};
use crate::{
    Deserialize, Parcel, Parcelable, ParcelableMetadata, Serialize, NON_NULL_PARCELABLE_FLAG,
    NULL_PARCELABLE_FLAG,
};

use downcast_rs::{impl_downcast, DowncastSync};
use std::any::Any;
use std::sync::{Arc, Mutex};

trait AnyParcelable: DowncastSync + Parcelable + std::fmt::Debug {}
impl_downcast!(sync AnyParcelable);
impl<T> AnyParcelable for T where T: DowncastSync + Parcelable + std::fmt::Debug {}

#[derive(Debug)]
enum ParcelableHolderData {
    Empty,
    Parcelable {
        parcelable: Arc<dyn AnyParcelable>,
        name: String,
    },
    Parcel(Parcel),
}

/// A type-erased container for any parcelable object.
///
/// `ParcelableHolder` can store any type implementing `Parcelable`, allowing
/// for runtime polymorphism over parcelable types. This is primarily used
/// for AIDL union types and generic parcelable handling.
///
/// Note: `ParcelableHolder` is currently not thread-safe (neither `Send` nor `Sync`)
/// due to its internal `Parcel` which is not thread-safe.
#[derive(Debug)]
pub struct ParcelableHolder {
    // This is a `Mutex` because of `get_parcelable`
    // which takes `&self` for consistency with C++.
    // We could make `get_parcelable` take a `&mut self`
    // and get rid of the `Mutex` here for a performance
    // improvement, but then callers would require a mutable
    // `ParcelableHolder` even for that getter method.
    data: Mutex<ParcelableHolderData>,
    stability: Stability,
}

impl Default for ParcelableHolder {
    fn default() -> Self {
        Self::new(Stability::Local)
    }
}

impl ParcelableHolder {
    /// Construct a new `ParcelableHolder` with the given stability.
    pub fn new(stability: Stability) -> Self {
        Self {
            data: Mutex::new(ParcelableHolderData::Empty),
            stability,
        }
    }

    /// Reset the contents of this `ParcelableHolder`.
    ///
    /// Note that this method does not reset the stability,
    /// only the contents.
    pub fn reset(&mut self) {
        *self
            .data
            .get_mut()
            .expect("Parcelable holder lock poisoned") = ParcelableHolderData::Empty;
        // We could also clear stability here, but C++ doesn't
    }

    /// Set the parcelable contained in this `ParcelableHolder`.
    pub fn set_parcelable<T>(&mut self, p: Arc<T>) -> Result<()>
    where
        T: Any + Parcelable + ParcelableMetadata + std::fmt::Debug + Send + Sync,
    {
        if !p.stability().includes(self.stability) {
            log::error!(
                "ParcelableHolder::set_parcelable: parcelable stability {:?} does not include holder stability {:?}",
                p.stability(),
                self.stability
            );
            return Err(StatusCode::BadValue);
        }

        *self
            .data
            .get_mut()
            .expect("Parcelable holder lock poisoned") = ParcelableHolderData::Parcelable {
            parcelable: p,
            name: T::descriptor().into(),
        };

        Ok(())
    }

    /// Retrieve the parcelable stored in this `ParcelableHolder`.
    ///
    /// This method attempts to retrieve the parcelable inside
    /// the current object as a parcelable of type `T`.
    /// The object is validated against `T` by checking that
    /// its parcelable descriptor matches the one returned
    /// by `T::descriptor()`.
    ///
    /// Returns one of the following:
    /// * `Err(_)` in case of error
    /// * `Ok(None)` if the holder is empty or the descriptor does not match
    /// * `Ok(Some(_))` if the object holds a parcelable of type `T`
    ///   with the correct descriptor
    pub fn get_parcelable<T>(&self) -> Result<Option<Arc<T>>>
    where
        T: Any + Parcelable + ParcelableMetadata + Default + std::fmt::Debug + Send + Sync,
    {
        let parcelable_desc = T::descriptor();
        let mut data = self.data.lock().expect("Parcelable holder lock poisoned");
        match *data {
            ParcelableHolderData::Empty => Ok(None),
            ParcelableHolderData::Parcelable {
                ref parcelable,
                ref name,
            } => {
                if name != parcelable_desc {
                    log::error!(
                        "ParcelableHolder::get_parcelable: parcelable descriptor mismatch: {name:?} != {parcelable_desc:?}");
                    return Err(StatusCode::BadValue);
                }

                match Arc::clone(parcelable).downcast_arc::<T>() {
                    Err(_) => {
                        log::error!("ParcelableHolder::get_parcelable: parcelable type mismatch: {parcelable:?} != {parcelable_desc:?}");
                        Err(StatusCode::BadValue)
                    }
                    Ok(x) => Ok(Some(x)),
                }
            }
            ParcelableHolderData::Parcel(ref mut parcel) => {
                // Safety: 0 should always be a valid position.
                parcel.set_data_position(0);

                let name: String = parcel.read()?;
                if name != parcelable_desc {
                    return Ok(None);
                }

                let mut parcelable = T::default();
                parcelable.read_from_parcel(parcel)?;

                let parcelable = Arc::new(parcelable);
                let result = Arc::clone(&parcelable);
                *data = ParcelableHolderData::Parcelable { parcelable, name };

                Ok(Some(result))
            }
        }
    }

    /// Return the stability value of this object.
    pub fn get_stability(&self) -> Stability {
        self.stability
    }
}

impl Serialize for ParcelableHolder {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write(&NON_NULL_PARCELABLE_FLAG)?;
        self.write_to_parcel(parcel)
    }
}

impl Deserialize for ParcelableHolder {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let status: i32 = parcel.read()?;
        if status == NULL_PARCELABLE_FLAG {
            log::error!("ParcelableHolder::deserialize: unexpected null");
            Err(StatusCode::UnexpectedNull)
        } else {
            let mut parcelable = ParcelableHolder::default();
            parcelable.read_from_parcel(parcel)?;
            Ok(parcelable)
        }
    }

    /// Read ONTO `self`, preserving its already-set stability. Plain
    /// `deserialize()` constructs a fresh `Local` holder, which then rejects a
    /// higher wire stability — losing the `@VintfStability` (or vendor/system)
    /// level a generated parcelable's `Default` assigned to a holder field.
    /// Generated `read_from_parcel` reads holder fields via `read_onto` so this
    /// override runs; mirrors AOSP's `field.readFromParcel(parcel)`.
    fn deserialize_from(&mut self, parcel: &mut Parcel) -> Result<()> {
        let status: i32 = parcel.read()?;
        if status == NULL_PARCELABLE_FLAG {
            log::error!("ParcelableHolder::deserialize_from: unexpected null");
            Err(StatusCode::UnexpectedNull)
        } else {
            self.read_from_parcel(parcel)
        }
    }
}

impl Parcelable for ParcelableHolder {
    fn write_to_parcel(&self, parcel: &mut Parcel) -> Result<()> {
        // AOSP serializes the stability Level *bitmask* (0/3/12/63 via
        // `From<Stability> for i32`), not the enum discriminant. Using
        // `as i32` here wrote 0/1/2/3, so any non-Local holder (e.g. Vintf
        // wrote 3 instead of 63) was rejected with BAD_VALUE by a real
        // peer. Mirror the `.into()` used on the SIBinder path.
        let stability: i32 = self.stability.into();
        parcel.write(&stability)?;

        let mut data = self.data.lock().expect("Parcelable holder lock poisoned");
        match *data {
            ParcelableHolderData::Empty => parcel.write(&0i32),
            ParcelableHolderData::Parcelable {
                ref parcelable,
                ref name,
            } => {
                let length_start = parcel.data_position();
                parcel.write(&0i32)?;

                let data_start = parcel.data_position();
                parcel.write(name)?;
                parcelable.write_to_parcel(parcel)?;

                let end = parcel.data_position();
                // Safety: we got the position from `data_position`.
                parcel.set_data_position(length_start);

                assert!(end >= data_start);
                parcel.write(&((end - data_start) as i32))?;
                // Safety: we got the position from `data_position`.
                parcel.set_data_position(end);

                Ok(())
            }
            ParcelableHolderData::Parcel(ref mut p) => {
                parcel.write(&(p.data_size() as i32))?;
                parcel.append_all_from(p)
            }
        }
    }

    fn read_from_parcel(&mut self, parcel: &mut Parcel) -> Result<()> {
        let wire_stability: i32 = parcel.read()?;
        let local_stability: i32 = self.stability.into();
        if local_stability != wire_stability {
            log::error!(
                "ParcelableHolder::read_from_parcel: parcelable stability mismatch: {:?} != {:?}",
                self.stability,
                wire_stability
            );

            return Err(StatusCode::BadValue);
        }

        let data_size: i32 = parcel.read()?;
        if data_size < 0 {
            // C++ returns BAD_VALUE here,
            // while Java returns ILLEGAL_ARGUMENT
            return Err(StatusCode::BadValue);
        }
        if data_size == 0 {
            *self
                .data
                .get_mut()
                .expect("Parcelable holder lock poisoned") = ParcelableHolderData::Empty;
            return Ok(());
        }

        // TODO: C++ ParcelableHolder accepts sizes up to SIZE_MAX here, but we
        // only go up to i32::MAX because that's what our API uses everywhere
        let data_start: usize = parcel.data_position();
        let data_end: usize = data_start
            .checked_add(data_size as usize)
            .ok_or(StatusCode::BadValue)?;

        let mut new_parcel = Parcel::new();
        new_parcel.append_from(parcel, data_start, data_size as usize)?;
        *self
            .data
            .get_mut()
            .expect("Parcelable holder lock poisoned") = ParcelableHolderData::Parcel(new_parcel);

        // Safety: `append_from` checks if `data_size` overflows
        // `parcel` and returns `BAD_VALUE` if that happens. We also
        // explicitly check for negative and zero `data_size` above,
        // so `data_end` is guaranteed to be greater than `data_start`.
        parcel.set_data_position(data_end);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_holder_serializes_stability_bitmask_not_discriminant() {
        // A Vintf holder must serialize the AOSP Level bitmask (63), not
        // the enum discriminant (3). An empty holder writes the stability
        // followed by a 0 length; a real peer rejects a wrong stability
        // value with BAD_VALUE, so this guards wire compatibility.
        let holder = ParcelableHolder::new(Stability::Vintf);
        let mut parcel = Parcel::new();
        holder.write_to_parcel(&mut parcel).unwrap();

        parcel.set_data_position(0);
        let wire_stability: i32 = parcel.read().unwrap();
        assert_eq!(wire_stability, 0b111111, "Vintf must serialize as 63");
        let expected: i32 = Stability::Vintf.into();
        assert_eq!(wire_stability, expected);

        // And the round-trip back into a same-stability holder accepts it.
        parcel.set_data_position(0);
        let mut dst = ParcelableHolder::new(Stability::Vintf);
        dst.read_from_parcel(&mut parcel).unwrap();
    }
}
