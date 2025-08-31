// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use similar::{ChangeTag, TextDiff};
use std::error::Error;

fn aidl_generator(input: &str, expect: &str) -> Result<(), Box<dyn std::error::Error>> {
    let document = rsbinder_aidl::parse_document(input)?;
    let gen = rsbinder_aidl::Generator::new(false, false);
    let res = gen.document(&document)?;
    let diff = TextDiff::from_lines(res.1.trim(), expect.trim());
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "- ",
            ChangeTag::Insert => "+ ",
            ChangeTag::Equal => "  ",
        };
        print!("{sign}{change}");
    }
    assert_eq!(res.1.trim(), expect.trim());
    Ok(())
}

#[cfg(test)]
const UNION: &str = r#"
    @JavaDerive(toString=true, equals=true)
    @RustDerive(Clone=true, PartialEq=true)
    union Union {
        int[] ns = {};
        int n;
        int m;
        @utf8InCpp String s;
        @nullable IBinder ibinder;
        @utf8InCpp List<String> ss;
        ByteEnum be;

        const @utf8InCpp String S1 = "a string constant in union";
    }
"#;

#[cfg(test)]
const CONSTANT_EXPRESSION_ENUM: &str = r#"
        @Backing(type="int")
        enum ConstantExpressionEnum {
            // Should be all true / ones.
            // dec literals are either int or long
            decInt32_1 = (~(-1)) == 0,
            decInt32_2 = ~~(1 << 31) == (1 << 31),
            decInt64_1 = (~(-1L)) == 0,
            decInt64_2 = (~4294967295L) != 0,
            decInt64_3 = (~4294967295) != 0,
            decInt64_4 = ~~(1L << 63) == (1L << 63),

            // hex literals could be int or long
            // 0x7fffffff is int, hence can be negated
            hexInt32_1 = -0x7fffffff < 0,

            // 0x80000000 is int32_t max + 1
            hexInt32_2 = 0x80000000 < 0,

            // 0xFFFFFFFF is int32_t, not long; if it were long then ~(long)0xFFFFFFFF != 0
            hexInt32_3 = ~0xFFFFFFFF == 0,

            // 0x7FFFFFFFFFFFFFFF is long, hence can be negated
            hexInt64_1 = -0x7FFFFFFFFFFFFFFF < 0
        }
"#;

#[cfg(test)]
const INT_ENUM: &str = r#"
        @Backing(type="int")
        enum IntEnum {
            FOO = 1000,
            BAR = 2000,
            BAZ,
            /** @deprecated do not use this */
            QUX,
        }
"#;

#[cfg(test)]
const LONG_ENUM: &str = r#"
        @Backing(type="long")
        enum LongEnum {
            FOO = 100000000000,
            BAR = 200000000000,
            BAZ,
        }
"#;

#[cfg(test)]
const BYTE_ENUM: &str = r#"
        @Backing(type="byte")
        enum ByteEnum {
            // Comment about FOO.
            FOO = 1,
            BAR = 2,
            BAZ,
        }
"#;

#[test]
fn test_unions() -> Result<(), Box<dyn Error>> {
    aidl_generator(
        &(r#"
        package android.aidl.tests;

        @Backing(type="byte")
        enum ByteEnum {
            // Comment about FOO.
            FOO = 1,
            BAR = 2,
            BAZ,
        }

        "#
        .to_owned()
            + UNION),
        r#"
pub mod ByteEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#ByteEnum : [i8; 3] {
            r#FOO = 1,
            r#BAR = 2,
            r#BAZ = 3,
        }
    }
}
pub mod Union {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    #[derive(Clone,PartialEq)]
    pub enum r#Union {
        r#Ns(Vec<i32>),
        r#N(i32),
        r#M(i32),
        r#S(String),
        r#Ibinder(Option<rsbinder::SIBinder>),
        r#Ss(Vec<String>),
        r#Be(super::ByteEnum::ByteEnum),
    }
    pub const r#S1: &str = "a string constant in union";
    impl Default for r#Union {
        fn default() -> Self {
            Self::Ns(Default::default())
        }
    }
    impl rsbinder::Parcelable for r#Union {
        fn write_to_parcel(&self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match self {
                Self::r#Ns(v) => {
                    parcel.write(&0i32)?;
                    parcel.write(v)
                }
                Self::r#N(v) => {
                    parcel.write(&1i32)?;
                    parcel.write(v)
                }
                Self::r#M(v) => {
                    parcel.write(&2i32)?;
                    parcel.write(v)
                }
                Self::r#S(v) => {
                    parcel.write(&3i32)?;
                    parcel.write(v)
                }
                Self::r#Ibinder(v) => {
                    parcel.write(&4i32)?;
                    parcel.write(v)
                }
                Self::r#Ss(v) => {
                    parcel.write(&5i32)?;
                    parcel.write(v)
                }
                Self::r#Be(v) => {
                    parcel.write(&6i32)?;
                    parcel.write(v)
                }
            }
        }
        fn read_from_parcel(&mut self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            let tag: i32 = parcel.read()?;
            match tag {
                0 => {
                    let value: Vec<i32> = parcel.read()?;
                    *self = Self::r#Ns(value);
                    Ok(())
                }
                1 => {
                    let value: i32 = parcel.read()?;
                    *self = Self::r#N(value);
                    Ok(())
                }
                2 => {
                    let value: i32 = parcel.read()?;
                    *self = Self::r#M(value);
                    Ok(())
                }
                3 => {
                    let value: String = parcel.read()?;
                    *self = Self::r#S(value);
                    Ok(())
                }
                4 => {
                    let value: Option<rsbinder::SIBinder> = parcel.read()?;
                    *self = Self::r#Ibinder(value);
                    Ok(())
                }
                5 => {
                    let value: Vec<String> = parcel.read()?;
                    *self = Self::r#Ss(value);
                    Ok(())
                }
                6 => {
                    let value: super::ByteEnum::ByteEnum = parcel.read()?;
                    *self = Self::r#Be(value);
                    Ok(())
                }
                _ => Err(rsbinder::StatusCode::BadValue),
            }
        }
    }
    rsbinder::impl_serialize_for_parcelable!(r#Union);
    rsbinder::impl_deserialize_for_parcelable!(r#Union);
    impl rsbinder::ParcelableMetadata for r#Union {
        fn descriptor() -> &'static str { "android.aidl.tests.Union" }
    }
    rsbinder::declare_binder_enum! {
        Tag : [i32; 7] {
            r#ns = 0,
            r#n = 1,
            r#m = 2,
            r#s = 3,
            r#ibinder = 4,
            r#ss = 5,
            r#be = 6,
        }
    }
}
        "#,
    )?;

    Ok(())
}

#[test]
fn test_enums() -> Result<(), Box<dyn Error>> {
    aidl_generator(
        BYTE_ENUM,
        r##"
pub mod ByteEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#ByteEnum : [i8; 3] {
            r#FOO = 1,
            r#BAR = 2,
            r#BAZ = 3,
        }
    }
}
        "##,
    )?;

    aidl_generator(
        r##"
        enum BackendType {
            CPP,
            JAVA,
            NDK,
            RUST,
        }
        "##,
        r##"
pub mod BackendType {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#BackendType : [i8; 4] {
            r#CPP = 0,
            r#JAVA = 1,
            r#NDK = 2,
            r#RUST = 3,
        }
    }
}
        "##,
    )?;

    aidl_generator(
        CONSTANT_EXPRESSION_ENUM,
        r##"
pub mod ConstantExpressionEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#ConstantExpressionEnum : [i32; 10] {
            r#decInt32_1 = 1,
            r#decInt32_2 = 1,
            r#decInt64_1 = 1,
            r#decInt64_2 = 1,
            r#decInt64_3 = 1,
            r#decInt64_4 = 1,
            r#hexInt32_1 = 1,
            r#hexInt32_2 = 1,
            r#hexInt32_3 = 1,
            r#hexInt64_1 = 1,
        }
    }
}
        "##,
    )?;

    aidl_generator(
        INT_ENUM,
        r##"
pub mod IntEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#IntEnum : [i32; 4] {
            r#FOO = 1000,
            r#BAR = 2000,
            r#BAZ = 2001,
            r#QUX = 2002,
        }
    }
}
        "##,
    )?;

    aidl_generator(
        LONG_ENUM,
        r##"
pub mod LongEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#LongEnum : [i64; 3] {
            r#FOO = 100000000000,
            r#BAR = 200000000000,
            r#BAZ = 200000000001,
        }
    }
}
        "##,
    )?;

    aidl_generator(
        &(BYTE_ENUM.to_owned()
            + r##"
interface ITestService {
    ByteEnum RepeatByteEnum(ByteEnum token);
}
        "##),
        r##"
pub mod ByteEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#ByteEnum : [i8; 3] {
            r#FOO = 1,
            r#BAR = 2,
            r#BAZ = 3,
        }
    }
}
pub mod ITestService {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    pub trait ITestService: rsbinder::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "ITestService" }
        fn r#RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::status::Result<super::ByteEnum::ByteEnum>;
        fn getDefaultImpl() -> Option<ITestServiceDefaultRef> where Self: Sized {
            DEFAULT_IMPL.get().cloned()
        }
        fn setDefaultImpl(d: ITestServiceDefaultRef) -> ITestServiceDefaultRef where Self: Sized {
            DEFAULT_IMPL.get_or_init(|| d).clone()
        }
    }
    pub trait ITestServiceAsync<P>: rsbinder::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "ITestService" }
        fn r#RepeatByteEnum<'a>(&'a self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<super::ByteEnum::ByteEnum>>;
    }
    #[::async_trait::async_trait]
    pub trait ITestServiceAsyncService: rsbinder::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "ITestService" }
        async fn r#RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::status::Result<super::ByteEnum::ByteEnum>;
    }
    impl BnTestService
    {
        pub fn new_async_binder<T, R>(inner: T, rt: R) -> rsbinder::Strong<dyn ITestService>
        where
            T: ITestServiceAsyncService + Sync + Send + 'static,
            R: rsbinder::BinderAsyncRuntime + Send + Sync + 'static,
        {
            struct Wrapper<T, R> {
                _inner: T,
                _rt: R,
            }
            impl<T, R> rsbinder::Interface for Wrapper<T, R> where T: rsbinder::Interface, R: Send + Sync {
                fn as_binder(&self) -> rsbinder::SIBinder { self._inner.as_binder() }
                fn dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> rsbinder::Result<()> { self._inner.dump(_writer, _args) }
            }
            impl<T, R> BnTestServiceAdapter for Wrapper<T, R>
            where
                T: ITestServiceAsyncService + Sync + Send + 'static,
                R: rsbinder::BinderAsyncRuntime + Send + Sync + 'static,
            {
                fn as_sync(&self) -> &dyn ITestService {
                    self
                }
                fn as_async(&self) -> &dyn ITestServiceAsyncService {
                    &self._inner
                }
            }
            impl<T, R> ITestService for Wrapper<T, R>
            where
                T: ITestServiceAsyncService + Sync + Send + 'static,
                R: rsbinder::BinderAsyncRuntime + Send + Sync + 'static,
            {
                fn r#RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::status::Result<super::ByteEnum::ByteEnum> {
                    self._rt.block_on(self._inner.r#RepeatByteEnum(_arg_token))
                }
            }
            let wrapped = Wrapper { _inner: inner, _rt: rt };
            let binder = rsbinder::native::Binder::new_with_stability(BnTestService(Box::new(wrapped)), rsbinder::Stability::default());
            rsbinder::Strong::new(Box::new(binder))
        }
    }
    pub trait ITestServiceDefault: Send + Sync {
        fn r#RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::status::Result<super::ByteEnum::ByteEnum> {
            Err(rsbinder::StatusCode::UnknownTransaction.into())
        }
    }
    pub(crate) mod transactions {
        pub(crate) const r#RepeatByteEnum: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;
    }
    pub type ITestServiceDefaultRef = std::sync::Arc<dyn ITestServiceDefault>;
    static DEFAULT_IMPL: std::sync::OnceLock<ITestServiceDefaultRef> = std::sync::OnceLock::new();
    rsbinder::declare_binder_interface! {
        ITestService["ITestService"] {
            native: {
                BnTestService(on_transact),
                adapter: BnTestServiceAdapter,
                r#async: ITestServiceAsyncService,
            },
            proxy: BpTestService,
            r#async: ITestServiceAsync,
        }
    }
    impl BpTestService {
        fn build_parcel_RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.binder.as_proxy().unwrap().prepare_transact(true)?;
            data.write(&_arg_token)?;
            Ok(data)
        }
        fn read_response_RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum, _aidl_reply: rsbinder::Result<Option<rsbinder::Parcel>>) -> rsbinder::status::Result<super::ByteEnum::ByteEnum> {
            if let Err(rsbinder::StatusCode::UnknownTransaction) = _aidl_reply {
                if let Some(_aidl_default_impl) = <Self as ITestService>::getDefaultImpl() {
                  return _aidl_default_impl.r#RepeatByteEnum(_arg_token);
                }
            }
            let mut _aidl_reply = _aidl_reply?.ok_or(rsbinder::StatusCode::UnexpectedNull)?;
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            if !_status.is_ok() { return Err(_status); }
            let _aidl_return: super::ByteEnum::ByteEnum = _aidl_reply.read()?;
            Ok(_aidl_return)
        }
    }
    impl ITestService for BpTestService {
        fn r#RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::status::Result<super::ByteEnum::ByteEnum> {
            let _aidl_data = self.build_parcel_RepeatByteEnum(_arg_token)?;
            let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::r#RepeatByteEnum, &_aidl_data, rsbinder::FLAG_CLEAR_BUF);
            self.read_response_RepeatByteEnum(_arg_token, _aidl_reply)
        }
    }
    impl<P: rsbinder::BinderAsyncPool> ITestServiceAsync<P> for BpTestService {
        fn r#RepeatByteEnum<'a>(&'a self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<super::ByteEnum::ByteEnum>> {
            let _aidl_data = match self.build_parcel_RepeatByteEnum(_arg_token) {
                Ok(_aidl_data) => _aidl_data,
                Err(err) => return Box::pin(std::future::ready(Err(err.into()))),
            };
            let binder = self.binder.clone();
            P::spawn(
                move || binder.as_proxy().unwrap().submit_transact(transactions::r#RepeatByteEnum, &_aidl_data, rsbinder::FLAG_CLEAR_BUF | rsbinder::FLAG_PRIVATE_LOCAL),
                move |_aidl_reply| async move {
                    self.read_response_RepeatByteEnum(_arg_token, _aidl_reply)
                }
            )
        }
    }
    impl<P: rsbinder::BinderAsyncPool> ITestServiceAsync<P> for rsbinder::Binder<BnTestService>
    {
        fn r#RepeatByteEnum<'a>(&'a self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<super::ByteEnum::ByteEnum>> {
            self.0.as_async().r#RepeatByteEnum(_arg_token)
        }
    }
    impl ITestService for rsbinder::Binder<BnTestService> {
        fn r#RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::status::Result<super::ByteEnum::ByteEnum> {
            self.0.as_sync().r#RepeatByteEnum(_arg_token)
        }
    }
    fn on_transact(
        _service: &dyn ITestService, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
        match _code {
            transactions::r#RepeatByteEnum => {
                let _arg_token: super::ByteEnum::ByteEnum = _reader.read()?;
                let _aidl_return = _service.r#RepeatByteEnum(_arg_token);
                match &_aidl_return {
                    Ok(_aidl_return) => {
                        _reply.write(&rsbinder::Status::from(rsbinder::StatusCode::Ok))?;
                        _reply.write(_aidl_return)?;
                    }
                    Err(_aidl_status) => {
                        _reply.write(_aidl_status)?;
                    }
                }
                Ok(())
            }
            _ => Err(rsbinder::StatusCode::UnknownTransaction),
        }
    }
}
        "##,
    )?;

    Ok(())
}
