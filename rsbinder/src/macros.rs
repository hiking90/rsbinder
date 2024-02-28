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
                fn as_async(&self) -> &dyn $native_async;
            }

            pub struct $native(Box<dyn $native_adapter + Send + Sync + 'static>);

            impl $native {
                /// Create a new binder service.
                pub fn new_binder<T: $interface + Sync + Send + 'static>(inner: T) -> $crate::Strong<dyn $interface> {
                    struct Wrapper<T> {
                        _inner: T,
                    }
                    impl<T> $native_adapter for Wrapper<T>
                    where
                        T: $interface + Sync + Send + 'static,
                    {
                        fn as_sync(&self) -> &dyn $interface { &self._inner }
                        fn as_async(&self) -> &dyn $native_async {
                            unreachable!("{} doesn't support async interface.", stringify!($interface))
                        }
                    }
                    let binder = $crate::native::Binder::new_with_stability($native(Box::new(Wrapper {_inner: inner})), $stability);
                    $crate::Strong::new(Box::new(binder))
                }
            }

            impl $crate::Remotable for $native {
                fn descriptor() -> &'static str where Self: Sized {
                    $descriptor
                }

                fn on_transact(&self, code: $crate::TransactionCode, reader: &mut $crate::Parcel, reply: &mut $crate::Parcel) -> $crate::Result<()> {
                    $on_transact(self.0.as_sync(), code, reader, reply)
                }

                fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> $crate::Result<()> {
                    self.0.as_sync().dump(_writer, _args)
                }
            }
        )?

        $(
            // Async interface trait implementations.
            impl<P: $crate::BinderAsyncPool> $crate::FromIBinder for dyn $async_interface<P> {
                fn try_from(ibinder: $crate::SIBinder) -> std::result::Result<$crate::Strong<dyn $async_interface<P>>, $crate::StatusCode> {
                    match <$proxy as $crate::Proxy>::from_binder(ibinder.clone()) {
                        Some(proxy) => Ok($crate::Strong::new(Box::new(proxy))),
                        None => {
                            match $crate::native::Binder::<$native>::try_from(ibinder) {
                                Ok(native) => {
                                    Ok($crate::Strong::new(Box::new(native.clone())))
                                }
                                Err(err) => Err(err),
                            }
                        }
                    }
                }
            }

            impl<P: $crate::BinderAsyncPool> $crate::Serialize for dyn $async_interface<P> + '_ {
                fn serialize(&self, parcel: &mut $crate::Parcel) -> std::result::Result<(), $crate::StatusCode> {
                    let binder = $crate::Interface::as_binder(self);
                    parcel.write(&binder)
                }
            }

            impl<P: $crate::BinderAsyncPool> $crate::SerializeOption for dyn $async_interface<P> + '_ {
                fn serialize_option(this: Option<&Self>, parcel: &mut $crate::Parcel) -> std::result::Result<(), $crate::StatusCode> {
                    parcel.write(&this.map($crate::Interface::as_binder))
                }
            }

            impl<P: $crate::BinderAsyncPool> std::fmt::Debug for dyn $async_interface<P> + '_ {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
        pub struct $native(Box<dyn $interface + Send + Sync + 'static>);

        impl $native {
            /// Create a new binder service.
            pub fn new_binder<T: $interface + Sync + Send + 'static>(inner: T) -> $crate::Strong<dyn $interface> {
                let binder = $crate::native::Binder::new_with_stability($native(Box::new(inner)), $stability);
                $crate::Strong::new(Box::new(binder))
            }
        }

        impl $crate::Remotable for $native {
            fn descriptor() -> &'static str where Self: Sized {
                $descriptor
            }

            fn on_transact(&self, code: $crate::TransactionCode, reader: &mut $crate::Parcel, reply: &mut $crate::Parcel) -> $crate::Result<()> {
                $on_transact(&*self.0, code, reader, reply)
            }

            fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> $crate::Result<()> {
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
                @doc[concat!("A binder [`Remotable`]($crate::binder_impl::Remotable) that holds an [`", stringify!($interface), "`] object.")]
                native: {
                    $native($on_transact),
                    $(adapter: $native_adapter,)?
                    $(r#async: $native_async,)?
                },
                @doc[concat!("A binder [`Proxy`]($crate::binder_impl::Proxy) that holds an [`", stringify!($interface), "`] remote interface.")]
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

            fn from_binder(binder: $crate::SIBinder) -> std::option::Option<Self> {
                if binder.descriptor() != $descriptor {
                    return None
                }
                if let Some(_) = binder.as_proxy() {
                    Some(Self { binder, $($fname: $finit),* })
                } else {
                    None
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
                    Some(proxy) => Ok($crate::Strong::new(Box::new(proxy))),
                    None => {
                        match $crate::native::Binder::<$native>::try_from(binder) {
                            Ok(native) => Ok($crate::Strong::new(Box::new(native.clone()))),
                            Err(err) => Err(err),
                        }
                    }
                }
            }
        }

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

        impl $crate::Deserialize for $enum {
            fn deserialize(parcel: &mut $crate::Parcel) -> $crate::Result<Self> {
                let res = parcel.read().map(Self);
                res
            }
        }

        impl $crate::DeserializeArray for $enum {
            fn deserialize_array(parcel: &mut $crate::Parcel) -> $crate::Result<Option<Vec<Self>>> {
                let v: Option<Vec<$backing>> =
                    <$backing as $crate::DeserializeArray>::deserialize_array(parcel)?;
                Ok(v.map(|v| v.into_iter().map(Self).collect()))
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use crate::{Interface, TransactionCode, Result, Binder, Parcel};

    pub trait IEcho: Interface {
        fn echo(&self, echo: &str) -> Result<String>;
    }

    pub trait IEchoAsyncService: Interface {
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

    #[test]
    fn test_declare_binder_interface() {
        let _ = BnEcho::new_binder(EchoService {});
    }

    #[cfg(feature = "async")]
    #[test]
    fn test_try_from() {
        use async_trait::async_trait;

        pub trait IEcho : Interface + Send {
            fn echo(&self, echo: &str) -> crate::status::Result<String>;
        }
        pub trait IEchoAsync<P> : Interface + Send {
            fn echo<'a>(&'a self, echo: &'a str) -> crate::BoxFuture<'a, crate::status::Result<String>>;
        }
        #[async_trait]
        pub trait IEchoAsyncService : Interface + Send {
            async fn echo(&self, echo: &str) -> crate::status::Result<String>;
        }
        pub struct BpEcho {
            binder: crate::SIBinder,
        }
        impl IEcho for BpEcho {
            fn echo(&self, _echo: &str) -> crate::status::Result<String> {
                todo!()
            }
        }
        impl<P: crate::BinderAsyncPool> IEchoAsync<P> for BpEcho {
            fn echo<'a>(&'a self, _echo: &'a str) -> crate::BoxFuture<'a, crate::status::Result<String>> {
                P::spawn(
                    move || {0},
                    |_| async move { Ok("".to_string()) },
                )
            }
        }
        impl Interface for BpEcho {
            fn as_binder(&self) -> crate::SIBinder {
                self.binder.clone()
            }
        }
        impl crate::Proxy for BpEcho
        where
            BpEcho: IEcho,
        {
            fn descriptor() -> &'static str {
                "my.echo"
            }

            fn from_binder(binder: crate::SIBinder) -> std::option::Option<Self> {
                if binder.descriptor() != Self::descriptor() {
                    return None
                }
                if binder.as_proxy().is_some() {
                    Some(Self { binder })
                } else {
                    None
                }
            }
        }

        pub trait BnEchoAdapter: Send + Sync {
            fn as_sync(&self) -> &dyn IEcho;
            fn as_async(&self) -> &dyn IEchoAsyncService;
        }

        struct Wrapper<T, R> {
            inner: T,
            rt: R,
        }

        impl<T, R> Interface for Wrapper<T, R>
        where
            T: IEchoAsyncService + Sync + Send + 'static,
            R: crate::BinderAsyncRuntime + Send + Sync + 'static,
        {
            fn as_binder(&self) -> crate::SIBinder {
                self.inner.as_binder()
            }

            fn dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
                self.inner.dump(_writer, _args)
            }
        }

        impl<T, R> IEcho for Wrapper<T, R>
        where
            T: IEchoAsyncService + Sync + Send + 'static,
            R: crate::BinderAsyncRuntime + Send + Sync + 'static,
        {
            fn echo(&self, echo: &str) -> crate::status::Result<String> {
                self.rt.block_on(self.inner.echo(echo))
            }
        }

        impl<T, R> BnEchoAdapter for Wrapper<T, R>
        where
            T: IEchoAsyncService + Sync + Send + 'static,
            R: crate::BinderAsyncRuntime + Send + Sync + 'static,
        {
            fn as_sync(&self) -> &dyn IEcho {
                self
            }
            fn as_async(&self) -> &dyn IEchoAsyncService {
                &self.inner
            }
        }

        pub struct BnEcho(Box<dyn BnEchoAdapter>);

        impl BnEcho
        {
            /// Create a new binder service.
            pub fn new_binder<T, R>(inner: T, rt: R) -> crate::Strong<dyn IEcho>
            where
                T: IEchoAsyncService + Sync + Send + 'static,
                R: crate::BinderAsyncRuntime + Send + Sync + 'static,
            {
                let bn = BnEcho(Box::new(Wrapper { inner, rt }));
                let binder = crate::native::Binder::new_with_stability(bn, crate::Stability::default());
                crate::Strong::new(Box::new(binder))
            }
        }

        impl crate::Remotable for BnEcho
        {
            fn descriptor() -> &'static str where Self: Sized {
                "my.echo"
            }

            fn on_transact(&self, _code: crate::TransactionCode, _reader: &mut crate::Parcel, _reply: &mut crate::Parcel) -> crate::Result<()> {
                todo!()
            }

            fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> crate::Result<()> {
                Ok(())
            }
        }

        impl IEcho for crate::Binder<BnEcho>
        {
            fn echo(&self, echo: &str) -> crate::status::Result<String> {
                self.0.as_sync().echo(echo)
            }
        }

        impl<P: crate::BinderAsyncPool> IEchoAsync<P> for crate::Binder<BnEcho>
        {
            fn echo<'a>(&'a self, echo: &'a str) -> crate::BoxFuture<'a, crate::status::Result<String>> {
                self.0.as_async().echo(echo)
            }
        }

        impl crate::FromIBinder for dyn IEcho {
            fn try_from(binder: crate::SIBinder) -> crate::Result<crate::Strong<dyn IEcho>> {
                match <BpEcho as crate::Proxy>::from_binder(binder.clone()) {
                    Some(proxy) => Ok(crate::Strong::new(Box::new(proxy))),
                    None => {
                        match crate::native::Binder::<BnEcho>::try_from(binder) {
                            Ok(native) => Ok(crate::Strong::new(Box::new(native.clone()))),
                            Err(err) => Err(err),
                        }
                    }
                }
            }
        }

        impl<P: crate::BinderAsyncPool> crate::FromIBinder for dyn IEchoAsync<P>
        {
            fn try_from(binder: crate::SIBinder) -> crate::Result<crate::Strong<dyn IEchoAsync<P>>>
            {
                match <BpEcho as crate::Proxy>::from_binder(binder.clone()) {
                    Some(proxy) => Ok(crate::Strong::new(Box::new(proxy))),
                    None => {
                        match crate::native::Binder::<BnEcho>::try_from(binder) {
                            Ok(native) => {
                                Ok(crate::Strong::new(Box::new(native.clone())))
                            }
                            Err(err) => Err(err),
                        }
                        // Err(crate::StatusCode::BadType.into())
                    }
                }
            }
        }

        impl<P: crate::BinderAsyncPool> crate::ToAsyncInterface<P> for dyn IEcho {
            type Target = dyn IEchoAsync<P>;
        }

        impl<P: crate::BinderAsyncPool> crate::ToSyncInterface for dyn IEchoAsync<P> {
            type Target = dyn IEcho;
        }

        struct MyEcho {}
        impl Interface for MyEcho {}
        #[async_trait]
        impl IEchoAsyncService for MyEcho {
            async fn echo(&self, echo: &str) -> crate::status::Result<String> {
                Ok(echo.to_owned())
            }
        }

        struct MyRuntime {}
        impl crate::BinderAsyncRuntime for MyRuntime {
            fn block_on<F: std::future::Future>(&self, _future: F) -> F::Output {
                todo!()
            }
        }

        let _echo = BnEcho::new_binder(MyEcho {}, MyRuntime{});

        // echo.into_async::<Tokio>().echo("hello");

    }
}
