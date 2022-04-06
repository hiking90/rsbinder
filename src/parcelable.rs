use crate::sys::{binder_transaction_data_secctx, binder_transaction_data};
use crate::binder;
use crate::error::*;
use crate::parcel::{ReadableParcel, WritableParcel};

/// A struct whose instances can be written to a [`Parcel`].
// Might be able to hook this up as a serde backend in the future?
pub trait Serialize {
    /// Serialize this instance into the given [`Parcel`].
    fn serialize(&self, parcel: &mut WritableParcel<'_>) -> Result<()>;
}

/// A struct whose instances can be restored from a [`Parcel`].
// Might be able to hook this up as a serde backend in the future?
pub trait Deserialize: Sized {
    /// Deserialize an instance from the given [`Parcel`].
    fn deserialize(parcel: &mut ReadableParcel<'_>) -> Result<Self>;

    /// Deserialize an instance from the given [`Parcel`] onto the
    /// current object. This operation will overwrite the old value
    /// partially or completely, depending on how much data is available.
    fn deserialize_from(&mut self, parcel: &mut ReadableParcel<'_>) -> Result<()> {
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
            fn serialize(&self, parcel: &mut WritableParcel<'_>) -> Result<()> {
                parcel.write_data(&self.to_ne_bytes());
                Ok(())
            }
        }
    };

    {Deserialize, $ty:ty} => {
        impl Deserialize for $ty {
            fn deserialize(parcel: &mut ReadableParcel<'_>) -> Result<Self> {
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
            fn serialize(&self, parcel: &mut WritableParcel<'_>) -> Result<()> {
                let val: $to_ty = *self as _;
                parcel.write_data(&val.to_ne_bytes());
                Ok(())
            }
        }
    };

    {Deserialize, $to_ty:ty, $ty:ty} => {
        impl Deserialize for $ty {
            fn deserialize(parcel: &mut ReadableParcel<'_>) -> Result<Self> {
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
    fn deserialize(parcel: &mut ReadableParcel<'_>) -> Result<Self> {
        Ok(<i32>::from_ne_bytes(parcel.try_into()?) != 0)
    }
}

impl Serialize for bool {
    fn serialize(&self, parcel: &mut WritableParcel<'_>) -> Result<()> {
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
            fn serialize(&self, parcel: &mut WritableParcel<'_>) -> Result<()> {
                const SIZE: usize = std::mem::size_of::<$ty>();
                parcel.write_data(unsafe { std::mem::transmute::<&$ty, &[u8;SIZE]>(self) });
                Ok(())
            }
        }
    };

    {Deserialize, $ty:ty} => {
        impl Deserialize for $ty {
            fn deserialize(parcel: &mut ReadableParcel<'_>) -> Result<Self> {
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
    fn serialize(&self, parcel: &mut WritableParcel<'_>) -> Result<()> {
        parcel.write::<i32>(&(self.0.len() as i32))?;

        for ch16 in self.0.encode_utf16() {
            parcel.write_data(&ch16.to_ne_bytes());
        }
        Ok(())
    }
}

impl Deserialize for String16 {
    fn deserialize(parcel: &mut ReadableParcel<'_>) -> Result<Self> {
        let len = parcel.read::<i32>()?;

        let mut str16 = Vec::with_capacity(len as _);

        for _ in 0..len {
            str16.push(<u16>::from_ne_bytes(parcel.try_into()?));
        }

        Ok(String16(String::from_utf16(&str16)?))
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