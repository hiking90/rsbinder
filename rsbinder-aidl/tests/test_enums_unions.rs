// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use similar::{ChangeTag, TextDiff};
use std::error::Error;

fn aidl_generator(input: &str, expect: &str) -> Result<(), Box<dyn std::error::Error>> {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx)?;
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
            },
            proxy: BpTestService,
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
    impl ITestService for rsbinder::Binder<BnTestService> {
        fn r#RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::status::Result<super::ByteEnum::ByteEnum> {
            self.0.r#RepeatByteEnum(_arg_token)
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

// Tests for GitHub issue #67: Panic when generating for nested types in unions
// https://github.com/hiking90/rsbinder/issues/67

#[test]
fn test_union_with_nested_enum() -> Result<(), Box<dyn Error>> {
    // Exact reproduction case from issue #67
    aidl_generator(
        r#"
        package test;
        @VintfStability
        union NestedUnion {
            @VintfStability
            @Backing(type="int")
            enum Inner {
                VALUE = 0,
            }
            NestedUnion.Inner innerField;
        }
        "#,
        r#"
pub mod NestedUnion {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    pub enum r#NestedUnion {
        r#InnerField(Inner::Inner),
    }
    impl Default for r#NestedUnion {
        fn default() -> Self {
            Self::InnerField(Inner::Inner::VALUE)
        }
    }
    impl rsbinder::Parcelable for r#NestedUnion {
        fn write_to_parcel(&self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match self {
                Self::r#InnerField(v) => {
                    parcel.write(&0i32)?;
                    parcel.write(v)
                }
            }
        }
        fn read_from_parcel(&mut self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            let tag: i32 = parcel.read()?;
            match tag {
                0 => {
                    let value: Inner::Inner = parcel.read()?;
                    *self = Self::r#InnerField(value);
                    Ok(())
                }
                _ => Err(rsbinder::StatusCode::BadValue),
            }
        }
    }
    rsbinder::impl_serialize_for_parcelable!(r#NestedUnion);
    rsbinder::impl_deserialize_for_parcelable!(r#NestedUnion);
    impl rsbinder::ParcelableMetadata for r#NestedUnion {
        fn descriptor() -> &'static str { "test.NestedUnion" }
        fn stability(&self) -> rsbinder::Stability { rsbinder::Stability::Vintf }
    }
    rsbinder::declare_binder_enum! {
        Tag : [i32; 1] {
            r#innerField = 0,
        }
    }
    pub mod Inner {
        #![allow(non_upper_case_globals, non_snake_case)]
        rsbinder::declare_binder_enum! {
            r#Inner : [i32; 1] {
                r#VALUE = 0,
            }
        }
    }
}
        "#,
    )?;

    Ok(())
}

#[test]
fn test_union_with_nested_parcelable() -> Result<(), Box<dyn Error>> {
    aidl_generator(
        r#"
        package test;
        union OuterUnion {
            parcelable InnerData {
                int value;
                @utf8InCpp String name;
            }
            int simpleField;
            OuterUnion.InnerData dataField;
        }
        "#,
        r#"
pub mod OuterUnion {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    pub enum r#OuterUnion {
        r#SimpleField(i32),
        r#DataField(InnerData::InnerData),
    }
    impl Default for r#OuterUnion {
        fn default() -> Self {
            Self::SimpleField(Default::default())
        }
    }
    impl rsbinder::Parcelable for r#OuterUnion {
        fn write_to_parcel(&self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match self {
                Self::r#SimpleField(v) => {
                    parcel.write(&0i32)?;
                    parcel.write(v)
                }
                Self::r#DataField(v) => {
                    parcel.write(&1i32)?;
                    parcel.write(v)
                }
            }
        }
        fn read_from_parcel(&mut self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            let tag: i32 = parcel.read()?;
            match tag {
                0 => {
                    let value: i32 = parcel.read()?;
                    *self = Self::r#SimpleField(value);
                    Ok(())
                }
                1 => {
                    let value: InnerData::InnerData = parcel.read()?;
                    *self = Self::r#DataField(value);
                    Ok(())
                }
                _ => Err(rsbinder::StatusCode::BadValue),
            }
        }
    }
    rsbinder::impl_serialize_for_parcelable!(r#OuterUnion);
    rsbinder::impl_deserialize_for_parcelable!(r#OuterUnion);
    impl rsbinder::ParcelableMetadata for r#OuterUnion {
        fn descriptor() -> &'static str { "test.OuterUnion" }
    }
    rsbinder::declare_binder_enum! {
        Tag : [i32; 2] {
            r#simpleField = 0,
            r#dataField = 1,
        }
    }
    pub mod InnerData {
        #![allow(non_upper_case_globals, non_snake_case, dead_code)]
        #[derive(Debug)]
        pub struct InnerData {
            pub r#value: i32,
            pub r#name: String,
        }
        impl Default for InnerData {
            fn default() -> Self {
                Self {
                    r#value: Default::default(),
                    r#name: Default::default(),
                }
            }
        }
        impl rsbinder::Parcelable for InnerData {
            fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
                _parcel.sized_write(|_sub_parcel| {
                    _sub_parcel.write(&self.r#value)?;
                    _sub_parcel.write(&self.r#name)?;
                    Ok(())
                })
            }
            fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
                _parcel.sized_read(|_sub_parcel| {
                    self.r#value = _sub_parcel.read()?;
                    self.r#name = _sub_parcel.read()?;
                    Ok(())
                })
            }
        }
        rsbinder::impl_serialize_for_parcelable!(InnerData);
        rsbinder::impl_deserialize_for_parcelable!(InnerData);
        impl rsbinder::ParcelableMetadata for InnerData {
            fn descriptor() -> &'static str { "test.OuterUnion.InnerData" }
        }
    }
}
        "#,
    )?;

    Ok(())
}

#[test]
fn test_union_with_multiple_nested_types() -> Result<(), Box<dyn Error>> {
    aidl_generator(
        r#"
        package test;
        @VintfStability
        union MultiNestedUnion {
            @VintfStability
            @Backing(type="int")
            enum Status {
                OK = 0,
                ERROR = 1,
            }
            @VintfStability
            parcelable Metadata {
                int id;
            }
            int rawValue;
            MultiNestedUnion.Status statusField;
        }
        "#,
        r#"
pub mod MultiNestedUnion {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    pub enum r#MultiNestedUnion {
        r#RawValue(i32),
        r#StatusField(Status::Status),
    }
    impl Default for r#MultiNestedUnion {
        fn default() -> Self {
            Self::RawValue(Default::default())
        }
    }
    impl rsbinder::Parcelable for r#MultiNestedUnion {
        fn write_to_parcel(&self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match self {
                Self::r#RawValue(v) => {
                    parcel.write(&0i32)?;
                    parcel.write(v)
                }
                Self::r#StatusField(v) => {
                    parcel.write(&1i32)?;
                    parcel.write(v)
                }
            }
        }
        fn read_from_parcel(&mut self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            let tag: i32 = parcel.read()?;
            match tag {
                0 => {
                    let value: i32 = parcel.read()?;
                    *self = Self::r#RawValue(value);
                    Ok(())
                }
                1 => {
                    let value: Status::Status = parcel.read()?;
                    *self = Self::r#StatusField(value);
                    Ok(())
                }
                _ => Err(rsbinder::StatusCode::BadValue),
            }
        }
    }
    rsbinder::impl_serialize_for_parcelable!(r#MultiNestedUnion);
    rsbinder::impl_deserialize_for_parcelable!(r#MultiNestedUnion);
    impl rsbinder::ParcelableMetadata for r#MultiNestedUnion {
        fn descriptor() -> &'static str { "test.MultiNestedUnion" }
        fn stability(&self) -> rsbinder::Stability { rsbinder::Stability::Vintf }
    }
    rsbinder::declare_binder_enum! {
        Tag : [i32; 2] {
            r#rawValue = 0,
            r#statusField = 1,
        }
    }
    pub mod Status {
        #![allow(non_upper_case_globals, non_snake_case)]
        rsbinder::declare_binder_enum! {
            r#Status : [i32; 2] {
                r#OK = 0,
                r#ERROR = 1,
            }
        }
    }
    pub mod Metadata {
        #![allow(non_upper_case_globals, non_snake_case, dead_code)]
        #[derive(Debug)]
        pub struct Metadata {
            pub r#id: i32,
        }
        impl Default for Metadata {
            fn default() -> Self {
                Self {
                    r#id: Default::default(),
                }
            }
        }
        impl rsbinder::Parcelable for Metadata {
            fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
                _parcel.sized_write(|_sub_parcel| {
                    _sub_parcel.write(&self.r#id)?;
                    Ok(())
                })
            }
            fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
                _parcel.sized_read(|_sub_parcel| {
                    self.r#id = _sub_parcel.read()?;
                    Ok(())
                })
            }
        }
        rsbinder::impl_serialize_for_parcelable!(Metadata);
        rsbinder::impl_deserialize_for_parcelable!(Metadata);
        impl rsbinder::ParcelableMetadata for Metadata {
            fn descriptor() -> &'static str { "test.MultiNestedUnion.Metadata" }
            fn stability(&self) -> rsbinder::Stability { rsbinder::Stability::Vintf }
        }
    }
}
        "#,
    )?;

    Ok(())
}
