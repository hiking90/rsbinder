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

#[cfg(feature = "async")]
#[macro_export]
macro_rules! __declare_binder_interface {
    {
        $interface:path[$descriptor:expr] {
            native: {
                $native:ident($on_transact:path),
                $(adapter: $native_adapter:ident,)?
                $(r#async: $native_async:ident,)?
            },
            proxy: $proxy:ident {
                $($fname:ident: $fty:ty = $finit:expr),*
            },
            $(r#async: $async_interface:ident,)?
            stability: $stability:expr,
        }
    } => {
        $(
            pub trait $native_adapter {
                fn as_sync(&self) -> &dyn $interface;
                #[allow(dead_code)]
                fn as_async(&self) -> &dyn $native_async;
                /// `Some` only for an async-backed service; `None` for a
                /// sync-only one. Lets the async [`$crate::FromIBinder`] cast
                /// reject a sync-only local binder up front instead of letting
                /// [`Self::as_async`] panic when a method is later called.
                #[allow(dead_code)]
                fn try_as_async(&self) -> ::core::option::Option<&dyn $native_async>;
            }

            pub struct $native(::std::boxed::Box<dyn $native_adapter + ::core::marker::Send + ::core::marker::Sync + 'static>);

            impl $native {
                /// Create a new binder service.
                pub fn new_binder<T: $interface + ::core::marker::Sync + ::core::marker::Send + 'static>(inner: T) -> $crate::Strong<dyn $interface> {
                    Self::new_binder_with_features(inner, $crate::BinderFeatures::default())
                }

                /// Create a new binder service with explicit binder features.
                ///
                /// Equivalent to [`Self::new_binder`] but lets the caller opt into
                /// kernel-level features such as `set_requesting_sid`.
                /// See `rsbinder::BinderFeatures`.
                pub fn new_binder_with_features<T: $interface + ::core::marker::Sync + ::core::marker::Send + 'static>(
                    inner: T,
                    features: $crate::BinderFeatures,
                ) -> $crate::Strong<dyn $interface> {
                    struct Wrapper<T> {
                        _inner: T,
                    }
                    impl<T> $native_adapter for Wrapper<T>
                    where
                        T: $interface + ::core::marker::Sync + ::core::marker::Send + 'static,
                    {
                        fn as_sync(&self) -> &dyn $interface { &self._inner }
                        fn as_async(&self) -> &dyn $native_async {
                            // Unreachable: the async `FromIBinder` cast gates on
                            // `try_as_async()` (below) and refuses a sync-only
                            // native, so no `dyn Async` handle ever reaches here.
                            unreachable!("{} doesn't support async interface.", stringify!($interface))
                        }
                        fn try_as_async(&self) -> ::core::option::Option<&dyn $native_async> { ::core::option::Option::None }
                    }
                    let binder = $crate::native::Binder::new_with_stability_and_features(
                        $native(::std::boxed::Box::new(Wrapper {_inner: inner})),
                        $stability,
                        features,
                    );
                    $crate::Strong::new(::std::boxed::Box::new(binder))
                }
            }

            impl $crate::Remotable for $native {
                fn descriptor() -> &'static str where Self: Sized {
                    $descriptor
                }

                fn on_transact(&self, code: $crate::TransactionCode, reader: &mut $crate::Parcel, reply: &mut $crate::Parcel) -> $crate::Result<()> {
                    $on_transact(self.0.as_sync(), code, reader, reply)
                }

                fn on_dump(&self, _writer: &mut dyn ::std::io::Write, _args: &[::std::string::String]) -> $crate::Result<()> {
                    self.0.as_sync().dump(_writer, _args)
                }
            }
        )?

        $(
            // Async interface trait implementations.
            impl<P: $crate::BinderAsyncPool> $crate::FromIBinder for dyn $async_interface<P> {
                fn try_from(ibinder: $crate::SIBinder) -> ::std::result::Result<$crate::Strong<dyn $async_interface<P>>, $crate::StatusCode> {
                    match <$proxy as $crate::Proxy>::from_binder(ibinder.clone()) {
                        ::core::option::Option::Some(proxy) => ::core::result::Result::Ok($crate::Strong::new(::std::boxed::Box::new(proxy))),
                        ::core::option::Option::None => {
                            match $crate::native::Binder::<$native>::try_from(ibinder) {
                                ::core::result::Result::Ok(native) => {
                                    // A local binder can back the async view only if it
                                    // was published as an async service. A sync-only
                                    // service's adapter answers `None` here, so reject
                                    // the cast now rather than panic in `as_async()` at
                                    // the first method call (AOSP returns `BadType` too).
                                    // `Strong::into_async` still turns this `Err` into a
                                    // panic; `Strong::try_into_async` surfaces it.
                                    if native.0.try_as_async().is_some() {
                                        ::core::result::Result::Ok($crate::Strong::new(::std::boxed::Box::new(native)))
                                    } else {
                                        ::core::result::Result::Err($crate::StatusCode::BadType)
                                    }
                                }
                                ::core::result::Result::Err(err) => ::core::result::Result::Err(err),
                            }
                        }
                    }
                }
            }

            impl<P: $crate::BinderAsyncPool> $crate::Serialize for dyn $async_interface<P> + '_ {
                fn serialize(&self, parcel: &mut $crate::Parcel) -> ::std::result::Result<(), $crate::StatusCode> {
                    let binder = $crate::Interface::as_binder(self);
                    parcel.write(&binder)
                }
            }

            impl<P: $crate::BinderAsyncPool> $crate::SerializeOption for dyn $async_interface<P> + '_ {
                fn serialize_option(this: ::core::option::Option<&Self>, parcel: &mut $crate::Parcel) -> ::std::result::Result<(), $crate::StatusCode> {
                    parcel.write(&this.map($crate::Interface::as_binder))
                }
            }

            impl<P: $crate::BinderAsyncPool> ::std::fmt::Debug for dyn $async_interface<P> + '_ {
                fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                    f.pad(stringify!($async_interface))
                }
            }

            // / Convert a &dyn $async_interface to Strong<dyn $async_interface>
            // impl<P: $crate::BinderAsyncPool> std::borrow::ToOwned for dyn $async_interface<P> {
            //     type Owned = $crate::Strong<dyn $async_interface<P>>;
            //     fn to_owned(&self) -> Self::Owned {
            //         self.as_binder().into_interface()
            //             .expect(concat!("Error cloning interface ", stringify!($async_interface)))
            //     }
            // }

            impl<P: $crate::BinderAsyncPool> $crate::ToAsyncInterface<P> for dyn $interface {
                type Target = dyn $async_interface<P>;
            }

            impl<P: $crate::BinderAsyncPool> $crate::ToSyncInterface for dyn $async_interface<P> {
                type Target = dyn $interface;
            }
        )?
    };
}

#[cfg(not(feature = "async"))]
#[macro_export]
macro_rules! __declare_binder_interface {
    {
        $interface:path[$descriptor:expr] {
            native: {
                $native:ident($on_transact:path),
                $(adapter: $native_adapter:ident,)?
                $(r#async: $native_async:ident,)?
            },
            proxy: $proxy:ident {
                $($fname:ident: $fty:ty = $finit:expr),*
            },
            $(r#async: $async_interface:ident,)?
            stability: $stability:expr,
        }
    } => {
        pub struct $native(::std::boxed::Box<dyn $interface + ::core::marker::Send + ::core::marker::Sync + 'static>);

        impl $native {
            /// Create a new binder service.
            pub fn new_binder<T: $interface + ::core::marker::Sync + ::core::marker::Send + 'static>(inner: T) -> $crate::Strong<dyn $interface> {
                Self::new_binder_with_features(inner, $crate::BinderFeatures::default())
            }

            /// Create a new binder service with explicit binder features.
            ///
            /// Equivalent to [`Self::new_binder`] but lets the caller opt into
            /// kernel-level features such as `set_requesting_sid`.
            /// See `rsbinder::BinderFeatures`.
            pub fn new_binder_with_features<T: $interface + ::core::marker::Sync + ::core::marker::Send + 'static>(
                inner: T,
                features: $crate::BinderFeatures,
            ) -> $crate::Strong<dyn $interface> {
                let binder = $crate::native::Binder::new_with_stability_and_features(
                    $native(::std::boxed::Box::new(inner)),
                    $stability,
                    features,
                );
                $crate::Strong::new(::std::boxed::Box::new(binder))
            }
        }

        impl $crate::Remotable for $native {
            fn descriptor() -> &'static str where Self: Sized {
                $descriptor
            }

            fn on_transact(&self, code: $crate::TransactionCode, reader: &mut $crate::Parcel, reply: &mut $crate::Parcel) -> $crate::Result<()> {
                $on_transact(&*self.0, code, reader, reply)
            }

            fn on_dump(&self, _writer: &mut dyn ::std::io::Write, _args: &[::std::string::String]) -> $crate::Result<()> {
                self.0.dump(_writer, _args)
            }
        }
    };
}

/// Declare a binder interface.
///
/// This is mainly used internally by the AIDL compiler.
#[macro_export]
macro_rules! declare_binder_interface {
    {
        $interface:path[$descriptor:expr] {
            native: {
                $native:ident($on_transact:path),
                $(adapter: $native_adapter:ident,)?
                $(r#async: $native_async:ident,)?
            },
            proxy: $proxy:ident,
            $(r#async: $async_interface:ident,)?
        }
    } => {
        $crate::declare_binder_interface! {
            $interface[$descriptor] {
                native: {
                    $native($on_transact),
                    $(adapter: $native_adapter,)?
                    $(r#async: $native_async,)?
                },
                proxy: $proxy {},
                $(r#async: $async_interface,)?
                stability: $crate::Stability::default(),
            }
        }
    };

    {
        $interface:path[$descriptor:expr] {
            native: {
                $native:ident($on_transact:path),
                $(adapter: $native_adapter:ident,)?
                $(r#async: $native_async:ident,)?
            },
            proxy: $proxy:ident,
            $(r#async: $async_interface:ident,)?
            stability: $stability:expr,
        }
    } => {
        $crate::declare_binder_interface! {
            $interface[$descriptor] {
                native: {
                    $native($on_transact),
                    $(adapter: $native_adapter,)?
                    $(r#async: $native_async,)?
                },
                proxy: $proxy {},
                $(r#async: $async_interface,)?
                stability: $stability,
            }
        }
    };

    {
        $interface:path[$descriptor:expr] {
            native: {
                $native:ident($on_transact:path),
                $(adapter: $native_adapter:ident,)?
                $(r#async: $native_async:ident,)?
            },
            proxy: $proxy:ident {
                $($fname:ident: $fty:ty = $finit:expr),*
            },
            $(r#async: $async_interface:ident,)?
        }
    } => {
        $crate::declare_binder_interface! {
            $interface[$descriptor] {
                native: {
                    $native($on_transact),
                    $(adapter: $native_adapter,)?
                    $(r#async: $native_async,)?
                },
                proxy: $proxy {
                    $($fname: $fty = $finit),*
                },
                $(r#async: $async_interface,)?
                stability: $crate::Stability::default(),
            }
        }
    };

    {
        $interface:path[$descriptor:expr] {
            native: {
                $native:ident($on_transact:path),
                $(adapter: $native_adapter:ident,)?
                $(r#async: $native_async:ident,)?
            },
            proxy: $proxy:ident {
                $($fname:ident: $fty:ty = $finit:expr),*
            },
            $(r#async: $async_interface:ident,)?
            stability: $stability:expr,
        }
    } => {
        $crate::declare_binder_interface! {
            $interface[$descriptor] {
                @doc[concat!("A binder `Remotable` that holds an [`", stringify!($interface), "`] object.")]
                native: {
                    $native($on_transact),
                    $(adapter: $native_adapter,)?
                    $(r#async: $native_async,)?
                },
                @doc[concat!("A binder `Proxy` that holds an [`", stringify!($interface), "`] remote interface.")]
                proxy: $proxy {
                    $($fname: $fty = $finit),*
                },
                $(r#async: $async_interface,)?
                stability: $stability,
            }
        }
    };

    {
        $interface:path[$descriptor:expr] {
            @doc[$native_doc:expr]
            native: {
                $native:ident($on_transact:path),
                $(adapter: $native_adapter:ident,)?
                $(r#async: $native_async:ident,)?
            },
            @doc[$proxy_doc:expr]
            proxy: $proxy:ident {
                $($fname:ident: $fty:ty = $finit:expr),*
            },
            $( r#async: $async_interface:ident, )?

            stability: $stability:expr,
        }
    } => {
        #[doc = $proxy_doc]
        pub struct $proxy {
            binder: $crate::SIBinder,
            $($fname: $fty,)*
        }

        impl $crate::Interface for $proxy {
            fn as_binder(&self) -> $crate::SIBinder {
                self.binder.clone()
            }
        }

        impl $crate::Proxy for $proxy
        where
            $proxy: $interface,
        {
            fn descriptor() -> &'static str {
                $descriptor
            }

            fn from_binder(binder: $crate::SIBinder) -> ::core::option::Option<Self> {
                // An `RpcProxy` resolved from the RPC wire carries no
                // descriptor (the wire transmits only an address). Stamp
                // this stub's descriptor onto the *cached* proxy in
                // place — never a new proxy (that doubles the DEC_STRONG
                // and splits the dedup cache). Done before the descriptor
                // check so it then passes for RPC too. The shim is gated
                // inside rsbinder (no-op without `rpc`), so the kernel
                // path and `rpc`-off builds are byte-unaffected.
                $crate::__rpc_stamp_descriptor(&binder, $descriptor);
                // NOTE (RPC type-safety asymmetry): for a kernel
                // `ProxyHandle` the check below validates the *remote's*
                // interface (its descriptor comes from the driver). For
                // a fresh `RpcProxy` the wire carries no descriptor, so
                // the stamp above just wrote `$descriptor` — this check
                // is then self-referential and cannot reject a
                // wrong-interface cast. An `IBar` object cast to
                // `BpFoo` therefore succeeds here and surfaces only as a
                // transact-time `StatusCode` (the server rejects the
                // `IFoo` interface token), not as a `from_binder` `None`.
                // Inherent to the Android RPC wire; see
                // `RpcProxy::stamp_descriptor`'s one-address-one-
                // interface note.
                if binder.descriptor() != $descriptor {
                    return ::core::option::Option::None
                }
                if binder.as_remote().is_some() {
                    ::core::option::Option::Some(Self { binder, $($fname: $finit),* })
                } else {
                    ::core::option::Option::None
                }
            }
        }

        $crate::__declare_binder_interface!{
            $interface[$descriptor] {
                native: {
                    $native($on_transact),
                    $(adapter: $native_adapter,)?
                    $(r#async: $native_async,)?
                },
                proxy: $proxy {
                    $($fname: $fty = $finit),*
                },
                $(r#async: $async_interface,)?
                stability: $stability,
            }
        }

        impl $crate::FromIBinder for dyn $interface {
            fn try_from(binder: $crate::SIBinder) -> $crate::Result<$crate::Strong<dyn $interface>> {
                match <$proxy as $crate::Proxy>::from_binder(binder.clone()) {
                    ::core::option::Option::Some(proxy) => ::core::result::Result::Ok($crate::Strong::new(::std::boxed::Box::new(proxy))),
                    ::core::option::Option::None => {
                        match $crate::native::Binder::<$native>::try_from(binder) {
                            ::core::result::Result::Ok(native) => ::core::result::Result::Ok($crate::Strong::new(::std::boxed::Box::new(native))),
                            ::core::result::Result::Err(err) => ::core::result::Result::Err(err),
                        }
                    }
                }
            }
        }

        impl $crate::parcelable::Serialize for dyn $interface + '_
        where
            dyn $interface: $crate::Interface
        {
            fn serialize(&self, parcel: &mut $crate::Parcel) -> $crate::Result<()> {
                let binder = $crate::Interface::as_binder(self);
                parcel.write(&binder)?;
                ::core::result::Result::Ok(())
            }
        }

        impl $crate::parcelable::SerializeOption for dyn $interface + '_ {
            fn serialize_option(this: ::core::option::Option<&Self>, parcel: &mut $crate::Parcel) -> $crate::Result<()> {
                parcel.write(&this.map($crate::Interface::as_binder))
            }
        }

        impl ::std::fmt::Debug for dyn $interface + '_ {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
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
#[macro_export]
macro_rules! impl_serialize_for_parcelable {
    ($parcelable:ident) => {
        impl $crate::Serialize for $parcelable {
            fn serialize(&self, parcel: &mut $crate::Parcel) -> $crate::Result<()> {
                <Self as $crate::SerializeOption>::serialize_option(
                    ::core::option::Option::Some(self),
                    parcel,
                )
            }
        }

        impl $crate::SerializeArray for $parcelable {}

        impl $crate::SerializeOption for $parcelable {
            fn serialize_option(
                this: ::core::option::Option<&Self>,
                parcel: &mut $crate::Parcel,
            ) -> $crate::Result<()> {
                if let ::core::option::Option::Some(this) = this {
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
            fn deserialize(parcel: &mut $crate::Parcel) -> $crate::Result<Self> {
                $crate::DeserializeOption::deserialize_option(parcel)
                    .transpose()
                    .unwrap_or(::core::result::Result::Err(
                        $crate::StatusCode::UnexpectedNull.into(),
                    ))
            }
            fn deserialize_from(&mut self, parcel: &mut $crate::Parcel) -> $crate::Result<()> {
                let status: i32 = parcel.read()?;
                if status == $crate::NULL_PARCELABLE_FLAG {
                    ::core::result::Result::Err($crate::StatusCode::UnexpectedNull.into())
                } else if status == $crate::NON_NULL_PARCELABLE_FLAG {
                    use $crate::Parcelable;
                    self.read_from_parcel(parcel)
                } else {
                    // Any flag other than NON_NULL is UNEXPECTED_NULL, matching
                    // AOSP C++ `Parcel::readData` and `DeserializeOption`'s
                    // default path.
                    ::core::result::Result::Err($crate::StatusCode::UnexpectedNull.into())
                }
            }
        }

        impl $crate::DeserializeArray for $parcelable {}

        impl $crate::DeserializeOption for $parcelable {
            fn deserialize_option(
                parcel: &mut $crate::Parcel,
            ) -> $crate::Result<::core::option::Option<Self>> {
                let mut result = ::core::option::Option::None;
                Self::deserialize_option_from(&mut result, parcel)?;
                ::core::result::Result::Ok(result)
            }
            fn deserialize_option_from(
                this: &mut ::core::option::Option<Self>,
                parcel: &mut $crate::Parcel,
            ) -> $crate::Result<()> {
                let status: i32 = parcel.read()?;
                if status == $crate::NULL_PARCELABLE_FLAG {
                    *this = ::core::option::Option::None;
                    ::core::result::Result::Ok(())
                } else if status == $crate::NON_NULL_PARCELABLE_FLAG {
                    use $crate::Parcelable;
                    this.get_or_insert_with(Self::default)
                        .read_from_parcel(parcel)
                } else {
                    // Any flag other than NULL/NON_NULL is UNEXPECTED_NULL,
                    // matching AOSP C++ `Parcel::readData` and
                    // `DeserializeOption`'s default path.
                    ::core::result::Result::Err($crate::StatusCode::UnexpectedNull.into())
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

            #[inline(always)]
            #[allow(missing_docs)]
            pub const fn get(&self) -> $backing {
                self.0
            }
        }

        // Bitwise ops matching AOSP `declare_binder_enum!` (frameworks/native/libs/binder/rust/src/binder.rs); additive, wire-compatible.
        impl ::core::ops::BitOr for $enum {
            type Output = Self;
            fn bitor(self, rhs: Self) -> Self {
                Self(self.0 | rhs.0)
            }
        }

        impl ::core::ops::BitOrAssign for $enum {
            fn bitor_assign(&mut self, rhs: Self) {
                self.0 |= rhs.0;
            }
        }

        impl ::core::ops::BitAnd for $enum {
            type Output = Self;
            fn bitand(self, rhs: Self) -> Self {
                Self(self.0 & rhs.0)
            }
        }

        impl ::core::ops::BitAndAssign for $enum {
            fn bitand_assign(&mut self, rhs: Self) {
                self.0 &= rhs.0;
            }
        }

        impl ::core::ops::BitXor for $enum {
            type Output = Self;
            fn bitxor(self, rhs: Self) -> Self {
                Self(self.0 ^ rhs.0)
            }
        }

        impl ::core::ops::BitXorAssign for $enum {
            fn bitxor_assign(&mut self, rhs: Self) {
                self.0 ^= rhs.0;
            }
        }

        impl $crate::Serialize for $enum {
            fn serialize(&self, parcel: &mut $crate::Parcel) -> $crate::Result<()> {
                parcel.write(&self.0)
            }
        }

        impl $crate::SerializeArray for $enum {
            fn serialize_array(slice: &[Self], parcel: &mut $crate::Parcel) -> $crate::Result<()> {
                let v: ::std::vec::Vec<$backing> = slice.iter().map(|x| x.0).collect();
                <$backing as $crate::SerializeArray>::serialize_array(&v[..], parcel)
            }
        }

        impl $crate::Deserialize for $enum {
            fn deserialize(parcel: &mut $crate::Parcel) -> $crate::Result<Self> {
                let res = parcel.read().map(Self);
                res
            }
        }

        impl $crate::DeserializeArray for $enum {
            fn deserialize_array(parcel: &mut $crate::Parcel) -> $crate::Result<::core::option::Option<::std::vec::Vec<Self>>> {
                let v: ::core::option::Option<::std::vec::Vec<$backing>> =
                    <$backing as $crate::DeserializeArray>::deserialize_array(parcel)?;
                ::core::result::Result::Ok(v.map(|v| v.into_iter().map(Self).collect()))
            }
        }
    };
}

/// Include AIDL-generated Rust and (optionally) flatten an interface's items
/// into the current module — the one-call form of the
/// `include!(concat!(env!("OUT_DIR"), …))` + `pub use …::*` pair that every
/// AIDL consumer otherwise writes by hand.
///
/// Pass the **output file stem** you gave to `rsbinder_aidl::Builder::output`
/// (without the `.rs`). `env!("OUT_DIR")` and `include!` are expanded at the
/// call site, so the file resolves against the *consumer* crate's build
/// output — exactly as a hand-written `include!` would.
///
/// # Forms
///
/// ```ignore
/// // 1. include + re-export an interface's items (trait, Bn*, Bp*, …):
/// rsbinder::include_aidl!("hello", hello::IHello::*);
/// // expands to:
/// //   include!(concat!(env!("OUT_DIR"), "/hello.rs"));
/// //   pub use hello::IHello::*;
///
/// // 2. include only — re-export yourself (e.g. several interfaces in one file):
/// rsbinder::include_aidl!("multi");
/// pub use multi::IFoo::*;
/// pub use multi::IBar::*;
/// ```
///
/// The re-export path is taken verbatim as token trees, so it can be a glob
/// (`pkg::IFoo::*`), a selective list (`pkg::IFoo::{IFoo, BnFoo, BpFoo}`), or
/// start with `self::` inside a nested module.
///
/// The two-argument form emits `pub use`, so the interface's items become part
/// of the enclosing module's public surface. For any other visibility, use the
/// single-argument (include-only) form and write your own `use` / `pub use`.
///
/// # What it does not do
///
/// The interface name is not derivable from the file stem (`"hello"` →
/// `IHello` is only a naming convention), so the re-export path is still given
/// explicitly. The `build.rs` codegen step (`Builder`) is unaffected — this
/// replaces only the `lib.rs` include/`use` boilerplate.
#[macro_export]
macro_rules! include_aidl {
    ($file:literal, $($use_path:tt)+) => {
        include!(concat!(env!("OUT_DIR"), "/", $file, ".rs"));
        pub use $($use_path)+;
    };
    ($file:literal $(,)?) => {
        include!(concat!(env!("OUT_DIR"), "/", $file, ".rs"));
    };
}

#[cfg(test)]
mod tests {
    use crate::{Binder, Interface, Parcel, Result, TransactionCode};

    pub trait IEcho: Interface {
        fn echo(&self, echo: &str) -> Result<String>;
    }

    pub trait IEchoAsyncService: Interface {
        #[allow(dead_code)]
        fn echo(&self, echo: &str) -> Result<String>;
    }

    declare_binder_interface! {
        IEcho["my.echo"] {
            native: {
                BnEcho(on_transact),
                adapter: BnEchoAdapter,
                r#async: IEchoAsyncService,
            },
            proxy: BpEcho{},
        }
    }

    #[allow(dead_code)]
    impl IEcho for Binder<BnEcho> {
        #[cfg(feature = "async")]
        fn echo(&self, echo: &str) -> Result<String> {
            self.0.as_sync().echo(echo)
        }
        #[cfg(not(feature = "async"))]
        fn echo(&self, echo: &str) -> Result<String> {
            self.0.echo(echo)
        }
    }

    impl IEcho for BpEcho {
        fn echo(&self, _echo: &str) -> Result<String> {
            unimplemented!("BpEcho::echo")
        }
    }

    fn on_transact(
        _service: &dyn IEcho,
        _code: TransactionCode,
        _data: &mut Parcel,
        _reply: &mut Parcel,
    ) -> Result<()> {
        // ...
        Ok(())
    }

    struct EchoService {}

    impl Interface for EchoService {}

    impl IEcho for EchoService {
        fn echo(&self, echo: &str) -> Result<String> {
            Ok(echo.to_owned())
        }
    }

    // A second, unrelated interface used only to exercise a wrong-type cast.
    // Mirrors `IEcho` (adapter + async) so it is a fully-formed interface in
    // both sync and async builds.
    pub trait IBye: Interface {
        #[allow(dead_code)]
        fn bye(&self) -> Result<()>;
    }

    pub trait IByeAsyncService: Interface {
        #[allow(dead_code)]
        fn bye(&self) -> Result<()>;
    }

    declare_binder_interface! {
        IBye["my.bye"] {
            native: {
                BnBye(on_transact_bye),
                adapter: BnByeAdapter,
                r#async: IByeAsyncService,
            },
            proxy: BpBye{},
        }
    }

    #[allow(dead_code)]
    impl IBye for Binder<BnBye> {
        #[cfg(feature = "async")]
        fn bye(&self) -> Result<()> {
            self.0.as_sync().bye()
        }
        #[cfg(not(feature = "async"))]
        fn bye(&self) -> Result<()> {
            self.0.bye()
        }
    }

    impl IBye for BpBye {
        fn bye(&self) -> Result<()> {
            unimplemented!("BpBye::bye")
        }
    }

    fn on_transact_bye(
        _service: &dyn IBye,
        _code: TransactionCode,
        _data: &mut Parcel,
        _reply: &mut Parcel,
    ) -> Result<()> {
        Ok(())
    }

    struct ByeService {}

    impl Interface for ByeService {}

    impl IBye for ByeService {
        fn bye(&self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_declare_binder_interface() {
        let _ = BnEcho::new_binder(EchoService {});
    }

    // F6: a cross-interface cast must fail with `BadType` (not silently
    // succeed), and the single diagnostic funnel in `native::try_from` is what
    // names the expected vs. actual descriptor in the log.
    #[test]
    fn test_cast_mismatch_is_bad_type() {
        // `Into<SIBinder>` drops the interface type — no `.as_binder()`.
        let echo: crate::SIBinder = BnEcho::new_binder(EchoService {}).into();
        let bye: crate::SIBinder = BnBye::new_binder(ByeService {}).into();

        // The matching interface round-trips both ways.
        assert!(<dyn IEcho as crate::FromIBinder>::try_from(echo.clone()).is_ok());
        assert!(<dyn IBye as crate::FromIBinder>::try_from(bye.clone()).is_ok());

        // The wrong interface is rejected (symmetrically) as `BadType` — opaque
        // in the value, but with expected/actual descriptors in the diagnostic.
        assert_eq!(
            <dyn IBye as crate::FromIBinder>::try_from(echo).unwrap_err(),
            crate::StatusCode::BadType,
        );
        assert_eq!(
            <dyn IEcho as crate::FromIBinder>::try_from(bye).unwrap_err(),
            crate::StatusCode::BadType,
        );
    }

    // C1: `Strong::<dyn IFoo>::try_from(sib)` is the idiomatic cast spelling and
    // must behave exactly like `FromIBinder::try_from` / `into_interface`.
    #[test]
    fn test_strong_try_from() {
        let echo: crate::SIBinder = BnEcho::new_binder(EchoService {}).into();

        // The matching interface round-trips via the TryFrom impl.
        assert!(crate::Strong::<dyn IEcho>::try_from(echo.clone()).is_ok());

        // The wrong interface is rejected as `BadType`, matching the funnel.
        assert_eq!(
            crate::Strong::<dyn IBye>::try_from(echo).unwrap_err(),
            crate::StatusCode::BadType,
        );
    }

    // E4: link_to_death_arc accepts a concrete `Arc<R>` with no
    // `as Arc<dyn DeathRecipient>` cast, on both `Strong<I>` and `SIBinder`.
    // A native (local) binder rejects the link with InvalidOperation, which
    // exercises the unsizing + delegation end-to-end.
    #[test]
    fn test_link_to_death_arc_no_cast() {
        struct Rec;
        impl crate::DeathRecipient for Rec {
            fn binder_died(&self, _who: &crate::WIBinder) {}
        }
        let recipient = std::sync::Arc::new(Rec);

        // Strong<dyn IEcho> path (no cast at the call site).
        let strong = BnEcho::new_binder(EchoService {});
        assert_eq!(
            strong.link_to_death_arc(&recipient).unwrap_err(),
            crate::StatusCode::InvalidOperation,
        );

        // SIBinder path.
        let sib: crate::SIBinder = strong.into();
        assert_eq!(
            sib.link_to_death_arc(&recipient).unwrap_err(),
            crate::StatusCode::InvalidOperation,
        );
        assert_eq!(
            sib.unlink_to_death_arc(&recipient).unwrap_err(),
            crate::StatusCode::InvalidOperation,
        );
    }
}
