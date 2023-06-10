use std::sync::{Arc, Weak};

use crate::{
    IBinder,
    sys::*,
    error::*,
    process_state::*,
    proxy::*,
    binder::*,
    parcel::Parcel,
};


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
}


parcelable_primitives! {
    impl Serialize for i32;
    impl Deserialize for i32;

    impl Serialize for u32;
    impl Deserialize for u32;

    impl Serialize for f32;
    impl Deserialize for f32;

    impl Serialize for i64;
    impl Deserialize for i64;

    impl Serialize for u64;
    impl Deserialize for u64;

    impl Serialize for f64;
    impl Deserialize for f64;

    impl Serialize for u128;
    impl Deserialize for u128;
}

parcelable_primitives_ex! {
    impl Serialize for i8 = i32;
    impl Deserialize for i8 = i32;

    impl Serialize for u8 = i32;
    impl Deserialize for u8 = i32;

    impl Serialize for i16 = i32;
    impl Deserialize for u16 = i32;
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

#[derive(PartialEq, Debug)]
pub struct String16(pub String);

impl Serialize for String16 {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        let mut utf16 = Vec::with_capacity(self.0.len());

        for ch16 in self.0.encode_utf16() {
            utf16.push(ch16);
        }

        parcel.write::<i32>(&(utf16.len() as i32))?;
        parcel.write_data(
            unsafe {
                std::slice::from_raw_parts(utf16.as_ptr() as *const u8,
                    utf16.len() * std::mem::size_of::<u16>())
            }
        );
        parcel.write_data(&vec![0, 0]);

        Ok(())
    }
}

impl Deserialize for String16 {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let len = parcel.read::<i32>()?;

        let data = parcel.read_data((len as usize + 1) * std::mem::size_of::<u16>())?;
        let res = String::from_utf16(
            unsafe {
                std::slice::from_raw_parts(data.as_ptr() as *const u16, len as _)
            }
        )?;

        Ok(String16(res))
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

impl Serialize for Arc<dyn IBinder> {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {

        let sched_bits = if ProcessState::as_self().background_scheduling_disabled() == false {
            sched_policy_mask(SCHED_NORMAL, 19)
        } else {
            0
        };

        let obj = if self.is_remote() {
            let proxy = self.as_any().downcast_ref::<Proxy>().expect("Downcast to Proxy<Unknown>");

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
            let weak = Box::new(Arc::downgrade(self));

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


impl Deserialize for Arc<dyn IBinder> {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let flat: flat_binder_object = parcel.read()?;
        let _stability: i32 = parcel.read()?;

        unsafe {
            match flat.hdr.type_ {
                BINDER_TYPE_BINDER => {
                    let weak = Box::from_raw(flat.__bindgen_anon_1.binder as *mut Box<Weak<dyn IBinder>>);
                    Weak::upgrade(&weak)
                        .ok_or_else(|| Error::from(StatusCode::DeadObject))
                }

                BINDER_TYPE_HANDLE => {
                    let res = ProcessState::as_self()
                        .strong_proxy_for_handle(flat.__bindgen_anon_1.handle, Box::new(Unknown {}));
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