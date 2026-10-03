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
//!
//! # Stability on the wire
//!
//! A holder's stability is written as AOSP's `Parcelable::Stability` enum
//! (`STABILITY_LOCAL = 0`, `STABILITY_VINTF = 1`). This is a *different* wire value from the
//! binder-object `internal::Stability::Level` bitmask (0/3/12/63 via
//! `From<Stability> for i32`) used on the `writeStrongBinder` path, and it is
//! version-independent: `frameworks/native/libs/binder/ParcelableHolder.cpp` writes
//! `writeInt32(static_cast<int32_t>(getStability()))` unchanged on every Android version
//! (byte-identical between android-12 and android-16), with no `Category` repr or Android-12
//! `0x0c000000` adjustment. The AIDL `@VintfStability` annotation maps a holder field to
//! `STABILITY_VINTF` (`system/tools/aidl` `generate_cpp.cpp`); everything else is
//! `STABILITY_LOCAL`. The binder-object encoding would put 63 (`0x0c00003f` on Android 12) on
//! the wire where a real libbinder peer expects 1, and the peer rejects any `@VintfStability`
//! holder field with `BAD_VALUE`.
//!
//! # Data-only payloads
//!
//! A holder carries its payload in a sub-parcel built by `read_from_parcel`, and that
//! sub-parcel inherits the marshalling mode of the parcel it was cut from. Otherwise the
//! data-only decoder behind `crate::from_bytes` hands its bytes to a kernel-mode reader, and
//! `read_object`'s null-meta shortcut waves a null-pointer, null-cookie object through without
//! an offset-table entry: a forged `BINDER_TYPE_HANDLE` with handle 0 becomes a proxy to the
//! context manager (or panics on `ProcessState::as_self()` in a process that never initialized
//! the driver). Both contradict what `from_bytes` promises about bytes you did not write.
//!
//! Keeping the payload data-only is not enough on its own: serializing the holder into a
//! *kernel* parcel would copy those bytes back into a buffer whose reader trusts them, and
//! one kernel round trip would turn file bytes into a proxy for the context manager.
//! `append_from` refuses that copy, and the copy into an RPC session parcel too; a data-only
//! sink still accepts the holder.
//!
//! # Objects
//!
//! `Parcel::sub_parcel` builds the sub-parcel: besides the mode it keeps the session profile
//! and the RPC objects inside the payload, so a binder and a v1+ fd decode from it as from the
//! parcel it was cut from. It is a received parcel when its source is, and keeps each binder it
//! reads for as long as the holder keeps the payload, so a `get_parcelable` retried after a
//! failure reads the same binder again without owing the peer a second `DEC_STRONG` (the
//! `parcel` module doc "`append_from`" has the details). Relaying the undecoded payload into a session parcel is
//! `append_from`, whose per-profile rules are in the `parcel` module doc "`append_from`" and
//! in the struct rustdoc "Relaying undecoded bytes".

use crate::binder::Stability;
use crate::error::{Result, StatusCode};
use crate::{
    Deserialize, Parcel, Parcelable, ParcelableMetadata, Serialize, NON_NULL_PARCELABLE_FLAG,
    NULL_PARCELABLE_FLAG,
};

use std::any::Any;
use std::sync::{Arc, Mutex};

trait AnyParcelable: Parcelable + std::fmt::Debug + Send + Sync + 'static {
    // Upcast so `get_parcelable` can `Arc::downcast` through the trait object to the concrete type.
    fn into_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;
}
impl<T: Parcelable + std::fmt::Debug + Send + Sync + 'static> AnyParcelable for T {
    fn into_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

#[derive(Debug)]
enum ParcelableHolderData {
    Empty,
    Parcelable {
        parcelable: Arc<dyn AnyParcelable>,
        name: String,
    },
    Parcel(Box<Parcel>),
}

/// A type-erased container for any parcelable object.
///
/// `ParcelableHolder` can store any type implementing `Parcelable`, allowing
/// for runtime polymorphism over parcelable types. This is primarily used
/// for AIDL union types and generic parcelable handling.
///
/// `ParcelableHolder` is `Send + Sync`: its state sits behind a `Mutex` and
/// rsbinder's `Parcel` is plain owned data (unlike AOSP's, which wraps a raw
/// `AParcel` pointer). The `Mutex` exists because `get_parcelable` takes
/// `&self`, as in C++; taking `&mut self` would remove it, but then callers
/// would need a mutable holder even for that getter.
///
/// # Stability on the wire
///
/// A holder writes AOSP's `Parcelable::Stability` enum (`STABILITY_LOCAL = 0`,
/// `STABILITY_VINTF = 1`), not the binder-object `Stability` level bitmask
/// (0/3/12/63). A `@VintfStability` holder therefore puts `1` on the wire, and
/// a libbinder peer rejects any other value (63, or `0x0c00003f` on Android 12)
/// with `BAD_VALUE`. The values are checked against android-12 and android-16
/// `frameworks/native/libs/binder/ParcelableHolder.cpp` and `Parcelable.h`, and
/// `system/tools/aidl` `generate_cpp.cpp` (vintf holder field init). An
/// rsbinder-to-rsbinder round trip cannot catch a wrong value, because both
/// ends share the encoding; only a golden byte or libbinder interop does.
///
/// # Relaying undecoded bytes
///
/// A holder read from a parcel keeps the payload's bytes undecoded until
/// [`get_parcelable::<T>`](Self::get_parcelable) is called for the payload's
/// own type. Writing such a holder copies those bytes, and the copy has to
/// stay meaningful in the destination, so it succeeds only where the objects
/// in it can follow:
///
/// - Into a kernel parcel only from a kernel parcel. Bytes from an RPC
///   transaction or from `from_bytes` (the `rpc` feature) are `BadType`
///   rather than bytes a kernel reader would trust.
/// - Into an RPC session parcel only from a parcel of the same session
///   (AOSP `Parcel::appendFrom`); a holder read from a kernel parcel, from
///   `from_bytes` or from another session is `BadType`.
/// - Within one session, on the android-16 wire (v2), which records where
///   each binder and fd sits: every binder in the payload takes its own
///   reference and every fd is duplicated, as AOSP `appendFrom` does from
///   android-16.0.0_r4, so the relay may carry both.
/// - Within one session on the r34 wire or android-13+ v0/v1: `BadType` for
///   any non-empty payload, data-only ones included. These wires do not
///   record where a binder sits, so the copy could not take the references
///   its binders need, and cannot prove there are none.
///
/// Resolving first always works: [`get_parcelable::<T>`](Self::get_parcelable)
/// decodes the payload into a typed value, and that value re-encodes into any
/// parcel it could be written to directly. So whether a relay succeeds
/// depends on whether the holder has been resolved, not only on where its
/// bytes came from. A handler whose reply fails this way returns the status
/// to its caller; the session stays usable.
///
/// Reading is not limited this way: a holder read from an RPC parcel decodes
/// its binders on every wire, and its fds on v1 and v2 (r34 and v0 record no
/// fd position, so an fd in an undecoded payload cannot be decoded there).
#[derive(Debug)]
pub struct ParcelableHolder {
    // A `Mutex` because `get_parcelable` takes `&self`, as in C++; see the struct rustdoc.
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
    /// * `Err(StatusCode::BadValue)` if a parcelable already decoded or
    ///   set holds a different descriptor (AOSP `getParcelable`)
    /// * `Err(_)` in case of any other error
    /// * `Ok(None)` if the holder is empty, or holds an undecoded payload
    ///   whose descriptor does not match
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

                match Arc::clone(parcelable).into_any_arc().downcast::<T>() {
                    Err(_) => {
                        log::error!("ParcelableHolder::get_parcelable: parcelable type mismatch: {parcelable:?} != {parcelable_desc:?}");
                        Err(StatusCode::BadValue)
                    }
                    Ok(x) => Ok(Some(x)),
                }
            }
            ParcelableHolderData::Parcel(ref mut parcel) => {
                // Position 0 (rewind to start) is always valid.
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

    /// The undecoded payload parcel, if any, for a test of what `read_from_parcel` kept.
    #[cfg(all(test, feature = "rpc"))]
    pub(crate) fn with_payload_parcel<R>(&self, f: impl FnOnce(Option<&mut Parcel>) -> R) -> R {
        let mut data = self.data.lock().expect("Parcelable holder lock poisoned");
        match *data {
            ParcelableHolderData::Parcel(ref mut p) => f(Some(p)),
            _ => f(None),
        }
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
        } else if status == NON_NULL_PARCELABLE_FLAG {
            let mut parcelable = ParcelableHolder::default();
            parcelable.read_from_parcel(parcel)?;
            Ok(parcelable)
        } else {
            Err(StatusCode::UnexpectedNull)
        }
    }

    /// Read ONTO `self`, preserving its already-set stability. Plain
    /// `deserialize()` constructs a fresh `Local` holder, which then rejects a
    /// `@VintfStability` wire stability — losing the level a generated
    /// parcelable's `Default` assigned to a holder field. Generated
    /// `read_from_parcel` reads holder fields via `read_onto` so this override
    /// runs; mirrors AOSP's `field.readFromParcel(parcel)`.
    fn deserialize_from(&mut self, parcel: &mut Parcel) -> Result<()> {
        let status: i32 = parcel.read()?;
        if status == NULL_PARCELABLE_FLAG {
            log::error!("ParcelableHolder::deserialize_from: unexpected null");
            Err(StatusCode::UnexpectedNull)
        } else if status == NON_NULL_PARCELABLE_FLAG {
            self.read_from_parcel(parcel)
        } else {
            Err(StatusCode::UnexpectedNull)
        }
    }
}

/// AOSP `Parcelable::Stability` (0 local, 1 VINTF), not the binder bitmask; see module doc.
fn parcelable_stability_repr(stability: Stability) -> i32 {
    match stability {
        Stability::Vintf => 1, // STABILITY_VINTF
        _ => 0,                // STABILITY_LOCAL
    }
}

impl Parcelable for ParcelableHolder {
    fn write_to_parcel(&self, parcel: &mut Parcel) -> Result<()> {
        let stability = parcelable_stability_repr(self.stability);
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
                // The position came from `data_position`, so it is in range.
                parcel.set_data_position(length_start);

                assert!(end >= data_start);
                parcel.write(&((end - data_start) as i32))?;
                // The position came from `data_position`, so it is in range.
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
        let local_stability = parcelable_stability_repr(self.stability);
        if local_stability != wire_stability {
            log::error!(
                "ParcelableHolder::read_from_parcel: parcelable stability mismatch: {:?} != {:?}",
                self.stability,
                wire_stability
            );

            return Err(StatusCode::BadValue);
        }
        // AOSP `ParcelableHolder::readFromParcel`: any later failure leaves the holder empty.
        *self
            .data
            .get_mut()
            .expect("Parcelable holder lock poisoned") = ParcelableHolderData::Empty;

        let data_size: i32 = parcel.read()?;
        if data_size < 0 {
            // C++ returns BAD_VALUE here, while Java returns ILLEGAL_ARGUMENT.
            return Err(StatusCode::BadValue);
        }
        if data_size == 0 {
            return Ok(());
        }

        let data_start: usize = parcel.data_position();
        let data_end: usize = data_start
            .checked_add(data_size as usize)
            .ok_or(StatusCode::BadValue)?;

        // Same mode and objects as the source (module doc "Data-only payloads", "Objects").
        let new_parcel = parcel.sub_parcel(data_start, data_size as usize)?;
        *self
            .data
            .get_mut()
            .expect("Parcelable holder lock poisoned") =
            ParcelableHolderData::Parcel(Box::new(new_parcel));

        // `sub_parcel` bounded `data_size`, and it is positive, so `data_end` is in range.
        parcel.set_data_position(data_end);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holder_serializes_parcelable_stability_not_binder_level_bitmask() {
        // Golden bytes, as a symmetric round trip cannot catch a wrong value; see struct rustdoc.
        let vintf = ParcelableHolder::new(Stability::Vintf);
        let mut vp = Parcel::new();
        vintf.write_to_parcel(&mut vp).unwrap();
        vp.set_data_position(0);
        let vintf_wire: i32 = vp.read().unwrap();
        assert_eq!(
            vintf_wire, 1,
            "Vintf holder must serialize as STABILITY_VINTF (1)"
        );

        let local = ParcelableHolder::default();
        let mut lp = Parcel::new();
        local.write_to_parcel(&mut lp).unwrap();
        lp.set_data_position(0);
        let local_wire: i32 = lp.read().unwrap();
        assert_eq!(
            local_wire, 0,
            "Local holder must serialize as STABILITY_LOCAL (0)"
        );

        // Round-trip back into a same-stability holder accepts it.
        vp.set_data_position(0);
        let mut dst = ParcelableHolder::new(Stability::Vintf);
        dst.read_from_parcel(&mut vp).unwrap();

        // A Local holder still rejects a Vintf (1) wire value with BadValue.
        vp.set_data_position(0);
        let mut local_dst = ParcelableHolder::default();
        assert!(matches!(
            local_dst.read_from_parcel(&mut vp),
            Err(StatusCode::BadValue)
        ));
    }

    /// Non-nullable: only `NON_NULL_PARCELABLE_FLAG` (1) passes; 0 or garbage is `UnexpectedNull`.
    #[test]
    fn holder_rejects_null_and_garbage_sentinels() {
        for status in [NULL_PARCELABLE_FLAG, 2, -1] {
            let mut p = Parcel::new();
            p.write(&status).unwrap();
            p.set_data_position(0);
            assert!(
                matches!(
                    ParcelableHolder::deserialize(&mut p),
                    Err(StatusCode::UnexpectedNull)
                ),
                "status {status} must be rejected as UnexpectedNull",
            );
        }
    }

    /// The payload sub-parcel inherits data-only mode; see module doc "Data-only payloads".
    #[test]
    fn a_forged_object_inside_a_holder_never_becomes_a_binder() {
        #[derive(Debug, Default)]
        struct BinderCarrier {
            binder: Option<crate::SIBinder>,
        }
        impl ParcelableMetadata for BinderCarrier {
            fn descriptor() -> &'static str {
                "rsbinder.test.BinderCarrier"
            }
        }
        impl Parcelable for BinderCarrier {
            fn write_to_parcel(&self, parcel: &mut Parcel) -> Result<()> {
                parcel.write(&self.binder)
            }
            fn read_from_parcel(&mut self, parcel: &mut Parcel) -> Result<()> {
                self.binder = parcel.read()?;
                Ok(())
            }
        }

        // Descriptor, a null-pointer/cookie handle-0 `flat_binder_object`, then its stability i32.
        let mut payload = Parcel::new_data_only();
        payload
            .write(&BinderCarrier::descriptor().to_string())
            .unwrap();
        payload
            .write(&crate::binder_object::flat_binder_object::new_handle(0, 0))
            .unwrap();
        payload.write(&0i32).unwrap();
        let payload = payload.into_bytes().unwrap();

        let mut encoded = Parcel::new_data_only();
        encoded.write(&NON_NULL_PARCELABLE_FLAG).unwrap();
        encoded.write(&0i32).unwrap(); // STABILITY_LOCAL
        encoded.write(&(payload.len() as i32)).unwrap();
        encoded.write_aligned_data(&payload).unwrap();
        let encoded = encoded.into_bytes().unwrap();

        let holder: ParcelableHolder = crate::from_bytes(&encoded).expect("the holder decodes");
        assert_eq!(
            holder.get_parcelable::<BinderCarrier>().err(),
            Some(StatusCode::BadType),
            "the payload of a data-only holder must stay data-only"
        );
    }

    /// `append_from` refuses a data-only holder into a kernel parcel; see "Data-only payloads".
    #[test]
    fn a_data_only_holder_is_refused_by_a_kernel_parcel() {
        let mut payload = Parcel::new_data_only();
        payload
            .write(&"rsbinder.test.BinderCarrier".to_string())
            .unwrap();
        payload
            .write(&crate::binder_object::flat_binder_object::new_handle(0, 0))
            .unwrap();
        payload.write(&0i32).unwrap();
        let payload = payload.into_bytes().unwrap();

        let mut encoded = Parcel::new_data_only();
        encoded.write(&NON_NULL_PARCELABLE_FLAG).unwrap();
        encoded.write(&0i32).unwrap(); // STABILITY_LOCAL
        encoded.write(&(payload.len() as i32)).unwrap();
        encoded.write_aligned_data(&payload).unwrap();
        let encoded = encoded.into_bytes().unwrap();

        let holder: ParcelableHolder = crate::from_bytes(&encoded).expect("the holder decodes");

        let mut kernel = Parcel::new();
        assert_eq!(
            kernel.write(&holder).err(),
            Some(StatusCode::BadType),
            "a data-only payload must not be relayed into a kernel parcel"
        );

        let mut data_only = Parcel::new_data_only();
        data_only
            .write(&holder)
            .expect("a same-mode sink still accepts the holder");
    }

    /// AOSP `readFromParcel`: past the stability check, a failed read leaves the holder empty.
    #[test]
    fn a_failed_read_empties_the_holder() {
        #[derive(Debug, Default)]
        struct Value(i32);
        impl ParcelableMetadata for Value {
            fn descriptor() -> &'static str {
                "rsbinder.test.Value"
            }
        }
        impl Parcelable for Value {
            fn write_to_parcel(&self, parcel: &mut Parcel) -> Result<()> {
                parcel.write(&self.0)
            }
            fn read_from_parcel(&mut self, parcel: &mut Parcel) -> Result<()> {
                self.0 = parcel.read()?;
                Ok(())
            }
        }
        // (stability, payload size): wrong stability keeps the value, as AOSP returns first.
        for (stability, size, kept) in [(1i32, 0i32, true), (0, -1, false), (0, 64, false)] {
            let mut holder = ParcelableHolder::default();
            holder.set_parcelable(Arc::new(Value(9))).unwrap();
            let mut p = Parcel::new();
            p.write(&stability).unwrap();
            p.write(&size).unwrap();
            p.set_data_position(0);
            assert!(
                holder.read_from_parcel(&mut p).is_err(),
                "({stability}, {size})"
            );
            let value = holder.get_parcelable::<Value>().unwrap().map(|v| v.0);
            assert_eq!(value, kept.then_some(9), "({stability}, {size})");
        }
    }
}
