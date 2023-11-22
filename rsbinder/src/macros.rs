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

#[macro_export]
macro_rules! declare_binder_interface {
    {
        $interface:path[$descriptor:expr] {
            native: $native:ident($on_transact:path),
            proxy: $proxy:ident,
        }
    } => {
        $crate::declare_binder_interface! {
            $interface[$descriptor] {
                native: $native($on_transact),
                proxy: $proxy {},
                stability: $crate::binder_impl::Stability::default(),
            }
        }
    };

    {
        $interface:path[$descriptor:expr] {
            native: $native:ident($on_transact:path),
            proxy: $proxy:ident,
            stability: $stability:expr,
        }
    } => {
        $crate::declare_binder_interface! {
            $interface[$descriptor] {
                native: $native($on_transact),
                proxy: $proxy {},
                stability: $stability,
            }
        }
    };

    {
        $interface:path[$descriptor:expr] {
            native: $native:ident($on_transact:path),
            proxy: $proxy:ident {
                $($fname:ident: $fty:ty = $finit:expr),*
            },
        }
    } => {
        $crate::declare_binder_interface! {
            $interface[$descriptor] {
                native: $native($on_transact),
                proxy: $proxy {
                    $($fname: $fty = $finit),*
                },
                stability: $crate::binder_impl::Stability::default(),
            }
        }
    };

    {
        $interface:path[$descriptor:expr] {
            native: $native:ident($on_transact:path),
            proxy: $proxy:ident {
                $($fname:ident: $fty:ty = $finit:expr),*
            },
            stability: $stability:expr,
        }
    } => {
        $crate::declare_binder_interface! {
            $interface[$descriptor] {
                @doc[concat!("A binder [`Remotable`]($crate::binder_impl::Remotable) that holds an [`", stringify!($interface), "`] object.")]
                native: $native($on_transact),
                @doc[concat!("A binder [`Proxy`]($crate::binder_impl::Proxy) that holds an [`", stringify!($interface), "`] remote interface.")]
                proxy: $proxy {
                    $($fname: $fty = $finit),*
                },
                stability: $stability,
            }
        }
    };

    {
        $interface:path[$descriptor:expr] {
            @doc[$native_doc:expr]
            native: $native:ident($on_transact:path),

            @doc[$proxy_doc:expr]
            proxy: $proxy:ident {
                $($fname:ident: $fty:ty = $finit:expr),*
            },

            stability: $stability:expr,
        }
    } => {
        #[doc = $proxy_doc]
        pub struct $proxy {
            binder: $crate::StrongIBinder,
            handle: $crate::ProxyHandle,
            $($fname: $fty,)*
        }

        impl $crate::Interface for $proxy {
            fn as_binder(&self) -> $crate::StrongIBinder {
                self.binder.clone()
            }

            // fn box_clone(&self) -> Box<dyn $crate::Interface> { todo!() }
        }

        impl $crate::Proxy for $proxy
        // where
        //     $proxy: $interface,
        {
            fn descriptor() -> &'static str {
                $descriptor
            }

            fn from_binder(binder: $crate::StrongIBinder) -> $crate::Result<Self> {
                let proxy = binder.as_proxy().ok_or($crate::Error::from($crate::StatusCode::BadValue))?.clone();
                if proxy.descriptor() != Self::descriptor() {
                    Err($crate::StatusCode::BadType.into())
                } else {
                    Ok(Self { binder, handle: proxy, $($fname: $finit),* })
                }
            }
        }

        pub struct $native(Box<dyn $interface + Sync + Send + 'static>);

        // impl $native {
        //     /// Create a new binder service.
        //     pub fn new_binder<T: $interface + Sync + Send + 'static>(inner: T) -> std::sync::Arc<dyn $interface> {
        //         // let mut binder = $crate::binder_impl::Binder::new_with_stability($native(Box::new(inner)), $stability);
        //         // $crate::binder_impl::IBinderInternal::set_requesting_sid(&mut binder, features.set_requesting_sid);
        //         std::sync::Arc::new(Box::new(inner))
        //     }
        // }


        impl $crate::parcelable::Serialize for dyn $interface
        where
            dyn $interface: $crate::Interface
        {
            fn serialize(&self, parcel: &mut $crate::Parcel) -> $crate::Result<()> {
                let binder = $crate::Interface::as_binder(self);
                parcel.write(&binder)?;
                Ok(())
            }
        }

        impl $crate::parcelable::SerializeOption for dyn $interface {
            fn serialize_option(this: Option<&Self>, parcel: &mut $crate::Parcel) -> $crate::Result<()> {
                parcel.write(&this.map($crate::Interface::as_binder))
            }
        }

        impl std::fmt::Debug for dyn $interface + '_ {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.pad(stringify!($interface))
            }
        }
    }
}


/// Implement `Serialize` trait and friends for a parcelable
///
/// This is an internal macro used by the AIDL compiler to implement
/// `Serialize`, `SerializeArray` and `SerializeOption` for
/// structured parcelables. The target type must implement the
/// `Parcelable` trait.
/// ```
#[macro_export]
macro_rules! impl_serialize_for_parcelable {
    ($parcelable:ident) => {
        impl $crate::Serialize for $parcelable {
            fn serialize(
                &self,
                parcel: &mut $crate::Parcel,
            ) -> $crate::Result<()> {
                <Self as $crate::SerializeOption>::serialize_option(Some(self), parcel)
            }
        }

        impl $crate::SerializeArray for $parcelable {}

        impl $crate::SerializeOption for $parcelable {
            fn serialize_option(
                this: Option<&Self>,
                parcel: &mut $crate::Parcel,
            ) -> $crate::Result<()> {
                if let Some(this) = this {
                    use $crate::Parcelable;
                    parcel.write(&$crate::NON_NULL_PARCELABLE_FLAG)?;
                    this.write_to_parcel(parcel)
                } else {
                    parcel.write(&$crate::NULL_PARCELABLE_FLAG)
                }
            }
        }
    };
}


/// Implement `Deserialize` trait and friends for a parcelable
///
/// This is an internal macro used by the AIDL compiler to implement
/// `Deserialize`, `DeserializeArray` and `DeserializeOption` for
/// structured parcelables. The target type must implement the
/// `Parcelable` trait.
#[macro_export]
macro_rules! impl_deserialize_for_parcelable {
    ($parcelable:ident) => {
        impl $crate::Deserialize for $parcelable {
            fn deserialize(
                parcel: &mut $crate::Parcel,
            ) -> $crate::Result<Self> {
                $crate::DeserializeOption::deserialize_option(parcel)
                    .transpose()
                    .unwrap_or(Err($crate::StatusCode::UnexpectedNull.into()))
            }
            fn deserialize_from(
                &mut self,
                parcel: &mut $crate::Parcel,
            ) -> $crate::Result<()> {
                let status: i32 = parcel.read()?;
                if status == $crate::NULL_PARCELABLE_FLAG {
                    Err($crate::StatusCode::UnexpectedNull.into())
                } else {
                    use $crate::Parcelable;
                    self.read_from_parcel(parcel)
                }
            }
        }

        impl $crate::DeserializeArray for $parcelable {}

        impl $crate::DeserializeOption for $parcelable {
            fn deserialize_option(
                parcel: &mut $crate::Parcel,
            ) -> $crate::Result<Option<Self>> {
                let mut result = None;
                Self::deserialize_option_from(&mut result, parcel)?;
                Ok(result)
            }
            fn deserialize_option_from(
                this: &mut Option<Self>,
                parcel: &mut $crate::Parcel,
            ) -> $crate::Result<()> {
                let status: i32 = parcel.read()?;
                if status == $crate::NULL_PARCELABLE_FLAG {
                    *this = None;
                    Ok(())
                } else {
                    use $crate::Parcelable;
                    this.get_or_insert_with(Self::default)
                        .read_from_parcel(parcel)
                }
            }
        }
    };
}


/// Declare an AIDL enumeration.
///
/// This is mainly used internally by the AIDL compiler.
#[macro_export]
macro_rules! declare_binder_enum {
    {
        $( #[$attr:meta] )*
        $enum:ident : [$backing:ty; $size:expr] {
            $( $( #[$value_attr:meta] )* $name:ident = $value:expr, )*
        }
    } => {
        $( #[$attr] )*
        #[derive(Debug, Default, Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Hash)]
        #[allow(missing_docs)]
        pub struct $enum(pub $backing);
        impl $enum {
            $( $( #[$value_attr] )* #[allow(missing_docs)] pub const $name: Self = Self($value); )*

            #[inline(always)]
            #[allow(missing_docs)]
            pub const fn enum_values() -> [Self; $size] {
                [$(Self::$name),*]
            }
        }

        impl $crate::Serialize for $enum {
            fn serialize(&self, parcel: &mut $crate::Parcel) -> $crate::Result<()> {
                parcel.write(&self.0)
            }
        }

        impl $crate::SerializeArray for $enum {
            fn serialize_array(slice: &[Self], parcel: &mut $crate::Parcel) -> $crate::Result<()> {
                let v: Vec<$backing> = slice.iter().map(|x| x.0).collect();
                <$backing as $crate::SerializeArray>::serialize_array(&v[..], parcel)
            }
        }

        impl $crate::SerializeOption for $enum {
            fn serialize_option(this: Option<&Self>, parcel: &mut $crate::Parcel) -> $crate::Result<()> {
                todo!()
                // parcel.write(&self.map(|x| x.0))
            }
        }

        impl $crate::Deserialize for $enum {
            fn deserialize(parcel: &mut $crate::Parcel) -> $crate::Result<Self> {
                parcel.read().map(Self)
            }
        }

        impl $crate::DeserializeArray for $enum {
            fn deserialize_array(parcel: &mut $crate::Parcel) -> $crate::Result<Option<Vec<Self>>> {
                let v: Option<Vec<$backing>> =
                    <$backing as $crate::DeserializeArray>::deserialize_array(parcel)?;
                Ok(v.map(|v| v.into_iter().map(Self).collect()))
                // Ok(v.into_iter().map(Self).collect())
            }
        }

        impl $crate::DeserializeOption for $enum {
            fn deserialize_option(parcel: &mut $crate::Parcel) -> $crate::Result<Option<Self>> {
                todo!()
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use crate::{Interface, TransactionCode, Result};

    pub trait IServiceManager: Interface {
        // remote methods...
    }

    declare_binder_interface! {
        IServiceManager["android.os.IServiceManager"] {
            native: BnServiceManager(on_transact),
            proxy: BpServiceManager{},
        }
    }

    fn on_transact(
        service: &dyn IServiceManager,
        code: TransactionCode,
        // data: &BorrowedParcel,
        // reply: &mut BorrowedParcel,
    ) -> Result<()> {
        // ...
        Ok(())
    }

}
