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

#[test]
fn test_parcelable_vintf() -> Result<(), Box<dyn Error>> {
    aidl_generator(
        r##"
package android.aidl.tests.vintf;

@VintfStability
parcelable VintfExtendableParcelable {
    ParcelableHolder ext;
}
        "##,
        r#"
pub mod VintfExtendableParcelable {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    pub struct VintfExtendableParcelable {
        pub r#ext: rsbinder::ParcelableHolder,
    }
    impl Default for VintfExtendableParcelable {
        fn default() -> Self {
            Self {
                r#ext: rsbinder::ParcelableHolder::new(rsbinder::Stability::Vintf),
            }
        }
    }
    impl rsbinder::Parcelable for VintfExtendableParcelable {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                _sub_parcel.write(&self.r#ext)?;
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                self.r#ext = _sub_parcel.read()?;
                Ok(())
            })
        }
    }
    rsbinder::impl_serialize_for_parcelable!(VintfExtendableParcelable);
    rsbinder::impl_deserialize_for_parcelable!(VintfExtendableParcelable);
    impl rsbinder::ParcelableMetadata for VintfExtendableParcelable {
        fn descriptor() -> &'static str { "android.aidl.tests.vintf.VintfExtendableParcelable" }
        fn stability(&self) -> rsbinder::Stability { rsbinder::Stability::Vintf }
    }
}
        "#,
    )?;

    aidl_generator(
        r##"
package android.aidl.tests.vintf;

@VintfStability
parcelable VintfParcelable {
    int a;
}
        "##,
        r#"
pub mod VintfParcelable {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    pub struct VintfParcelable {
        pub r#a: i32,
    }
    impl Default for VintfParcelable {
        fn default() -> Self {
            Self {
                r#a: Default::default(),
            }
        }
    }
    impl rsbinder::Parcelable for VintfParcelable {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                _sub_parcel.write(&self.r#a)?;
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                self.r#a = _sub_parcel.read()?;
                Ok(())
            })
        }
    }
    rsbinder::impl_serialize_for_parcelable!(VintfParcelable);
    rsbinder::impl_deserialize_for_parcelable!(VintfParcelable);
    impl rsbinder::ParcelableMetadata for VintfParcelable {
        fn descriptor() -> &'static str { "android.aidl.tests.vintf.VintfParcelable" }
        fn stability(&self) -> rsbinder::Stability { rsbinder::Stability::Vintf }
    }
}
        "#,
    )?;
    Ok(())
}

#[test]
fn test_parcelable_const_name() -> Result<(), Box<dyn Error>> {
    aidl_generator(
        r##"
parcelable IServiceManager {
    const int DUMP_FLAG_PRIORITY_CRITICAL = 1 << 0;
    const int DUMP_FLAG_PRIORITY_HIGH = 1 << 1;
    const int DUMP_FLAG_PRIORITY_NORMAL = 1 << 2;
    const int DUMP_FLAG_PRIORITY_DEFAULT = 1 << 3;
    const int DUMP_FLAG_PRIORITY_ALL =
             DUMP_FLAG_PRIORITY_CRITICAL | DUMP_FLAG_PRIORITY_HIGH
             | DUMP_FLAG_PRIORITY_NORMAL | DUMP_FLAG_PRIORITY_DEFAULT;
    const int DUMP_FLAG_PROTO = 1 << 4;
}
        "##,
        r#"
pub mod IServiceManager {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    pub const r#DUMP_FLAG_PRIORITY_CRITICAL: i32 = 1;
    pub const r#DUMP_FLAG_PRIORITY_HIGH: i32 = 2;
    pub const r#DUMP_FLAG_PRIORITY_NORMAL: i32 = 4;
    pub const r#DUMP_FLAG_PRIORITY_DEFAULT: i32 = 8;
    pub const r#DUMP_FLAG_PRIORITY_ALL: i32 = 15;
    pub const r#DUMP_FLAG_PROTO: i32 = 16;
    #[derive(Debug)]
    pub struct IServiceManager {
    }
    impl Default for IServiceManager {
        fn default() -> Self {
            Self {
            }
        }
    }
    impl rsbinder::Parcelable for IServiceManager {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                Ok(())
            })
        }
    }
    rsbinder::impl_serialize_for_parcelable!(IServiceManager);
    rsbinder::impl_deserialize_for_parcelable!(IServiceManager);
    impl rsbinder::ParcelableMetadata for IServiceManager {
        fn descriptor() -> &'static str { "IServiceManager" }
    }
}
        "#,
    )?;
    Ok(())
}

#[test]
fn test_parcelable() -> Result<(), Box<dyn Error>> {
    aidl_generator(
        r##"
        package android.os;

        /**
         * Remote connection info associated with a declared service
         * @hide
         */
        parcelable ConnectionInfo {
            /**
             * IP address that the service is listening on.
             */
            @utf8InCpp String ipAddress;
            /**
             * Port number that the service is listening on. Actual value is an unsigned integer.
             */
            int port;
        }
        "##,
        r#"
pub mod ConnectionInfo {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    pub struct ConnectionInfo {
        pub r#ipAddress: String,
        pub r#port: i32,
    }
    impl Default for ConnectionInfo {
        fn default() -> Self {
            Self {
                r#ipAddress: Default::default(),
                r#port: Default::default(),
            }
        }
    }
    impl rsbinder::Parcelable for ConnectionInfo {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                _sub_parcel.write(&self.r#ipAddress)?;
                _sub_parcel.write(&self.r#port)?;
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                self.r#ipAddress = _sub_parcel.read()?;
                self.r#port = _sub_parcel.read()?;
                Ok(())
            })
        }
    }
    rsbinder::impl_serialize_for_parcelable!(ConnectionInfo);
    rsbinder::impl_deserialize_for_parcelable!(ConnectionInfo);
    impl rsbinder::ParcelableMetadata for ConnectionInfo {
        fn descriptor() -> &'static str { "android.os.ConnectionInfo" }
    }
}
        "#,
    )?;
    Ok(())
}

#[test]
fn test_byte_parcelable() -> Result<(), Box<dyn Error>> {
    aidl_generator(
        &(r#"
package android.aidl.tests;
parcelable StructuredParcelable {
    int[] shouldContainThreeFs;
    int f;
    @utf8InCpp String shouldBeJerry;
    ByteEnum shouldBeByteBar;
    IntEnum shouldBeIntBar;
    LongEnum shouldBeLongBar;
    ByteEnum[] shouldContainTwoByteFoos;
    IntEnum[] shouldContainTwoIntFoos;
    LongEnum[] shouldContainTwoLongFoos;

    String stringDefaultsToFoo = "foo";
    byte byteDefaultsToFour = 4;
    int intDefaultsToFive = 5;
    long longDefaultsToNegativeSeven = -7;
    boolean booleanDefaultsToTrue = true;
    char charDefaultsToC = '\'';
    float floatDefaultsToPi = 3.14f;
    double doubleWithDefault = -3.14e17;
    int[] arrayDefaultsTo123 = {
            1,
            2,
            3,
    };
    int[] arrayDefaultsToEmpty = {};

    boolean boolDefault;
    byte byteDefault;
    int intDefault;
    long longDefault;
    float floatDefault;
    double doubleDefault;

    // parse checks only
    double checkDoubleFromFloat = 3.14f;
    String[] checkStringArray1 = {"a", "b"};
    @utf8InCpp String[] checkStringArray2 = {"a", "b"};

    // Add test to verify corner cases
    int int32_min = -2147483648;
    int int32_max = 2147483647;
    long int64_max = 9223372036854775807;
    int hexInt32_neg_1 = 0xffffffff;

    @nullable IBinder ibinder;

    // Make sure we can send an empty parcelable
    @JavaDerive(toString=true, equals=true)
    @RustDerive(Clone=true, PartialEq=true)
    parcelable Empty {}

    Empty empty;

    // Constant expressions that evaluate to 1
    byte[] int8_1 = {
            1,
            0xffu8 + 1 == 0,
            255u8 + 1 == 0,
            0x80u8 == -128,
            // u8 type is reinterpreted as a signed type
            0x80u8 / 2 == -0x40u8,
    };
    int[] int32_1 = {
            (~(-1)) == 0,
            ~~(1 << 31) == (1 << 31),
            -0x7fffffff < 0,
            0x80000000 < 0,

            0x7fffffff == 2147483647,

            // Shifting for more than 31 bits are undefined. Not tested.
            (1 << 31) == 0x80000000,

            // Should be all true / ones.
            (1 + 2) == 3,
            (8 - 9) == -1,
            (9 * 9) == 81,
            (29 / 3) == 9,
            (29 % 3) == 2,
            (0xC0010000 | 0xF00D) == (0xC001F00D),
            (10 | 6) == 14,
            (10 & 6) == 2,
            (10 ^ 6) == 12,
            6 < 10,
            (10 < 10) == 0,
            (6 > 10) == 0,
            (10 > 10) == 0,
            19 >= 10,
            10 >= 10,
            5 <= 10,
            (19 <= 10) == 0,
            19 != 10,
            (10 != 10) == 0,
            (22 << 1) == 44,
            (11 >> 1) == 5,
            (1 || 0) == 1,
            (1 || 1) == 1,
            (0 || 0) == 0,
            (0 || 1) == 1,
            (1 && 0) == 0,
            (1 && 1) == 1,
            (0 && 0) == 0,
            (0 && 1) == 0,

            // precedence tests -- all 1s
            4 == 4,
            -4 < 0,
            0xffffffff == -1,
            4 + 1 == 5,
            2 + 3 - 4,
            2 - 3 + 4 == 3,
            1 == 4 == 0,
            1 && 1,
            1 || 1 && 0, // && higher than ||
            1 < 2,
            !!((3 != 4 || (2 < 3 <= 3 > 4)) >= 0),
            !(1 == 7) && ((3 != 4 || (2 < 3 <= 3 > 4)) >= 0),
            (1 << 2) >= 0,
            (4 >> 1) == 2,
            (8 << -1) == 4,
            (1 << 30 >> 30) == 1,
            (1 | 16 >> 2) == 5,
            (0x0f ^ 0x33 & 0x99) == 0x1e, // & higher than ^
            (~42 & (1 << 3 | 16 >> 2) ^ 7) == 3,
            (2 + 3 - 4 * -7 / (10 % 3)) - 33 == 0,
            (2 + (-3 & 4 / 7)) == 2,
            (((((1 + 0))))),
    };
    long[] int64_1 = {
            (~(-1)) == 0,
            (~4294967295) != 0,
            (~4294967295) != 0,
            ~~(1L << 63) == (1L << 63),
            -0x7FFFFFFFFFFFFFFF < 0,

            0x7fffffff == 2147483647,
            0xfffffffff == 68719476735,
            0xffffffffffffffff == -1,
            (0xfL << 32L) == 0xf00000000,
            (0xfL << 32) == 0xf00000000,
    };
    int hexInt32_pos_1 = -0xffffffff;
    int hexInt64_pos_1 = -0xfffffffffff < 0;

    ConstantExpressionEnum const_exprs_1;
    ConstantExpressionEnum const_exprs_2;
    ConstantExpressionEnum const_exprs_3;
    ConstantExpressionEnum const_exprs_4;
    ConstantExpressionEnum const_exprs_5;
    ConstantExpressionEnum const_exprs_6;
    ConstantExpressionEnum const_exprs_7;
    ConstantExpressionEnum const_exprs_8;
    ConstantExpressionEnum const_exprs_9;
    ConstantExpressionEnum const_exprs_10;

    // String expressions
    @utf8InCpp String addString1 = "hello"
            + " world!";
    @utf8InCpp String addString2 = "The quick brown fox jumps "
            + "over the lazy dog.";

    const int BIT0 = 0x1;
    const int BIT1 = 0x1 << 1;
    const int BIT2 = 0x1 << 2;
    int shouldSetBit0AndBit2;

    @nullable Union u;
    @nullable Union shouldBeConstS1;

    IntEnum defaultWithFoo = IntEnum.FOO;
}
        "#
        .to_owned()
            + CONSTANT_EXPRESSION_ENUM
            + BYTE_ENUM
            + INT_ENUM
            + LONG_ENUM
            + UNION),
        r#"
pub mod StructuredParcelable {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    pub const r#BIT0: i32 = 1;
    pub const r#BIT1: i32 = 2;
    pub const r#BIT2: i32 = 4;
    #[derive(Debug)]
    pub struct StructuredParcelable {
        pub r#shouldContainThreeFs: Vec<i32>,
        pub r#f: i32,
        pub r#shouldBeJerry: String,
        pub r#shouldBeByteBar: super::ByteEnum::ByteEnum,
        pub r#shouldBeIntBar: super::IntEnum::IntEnum,
        pub r#shouldBeLongBar: super::LongEnum::LongEnum,
        pub r#shouldContainTwoByteFoos: Vec<super::ByteEnum::ByteEnum>,
        pub r#shouldContainTwoIntFoos: Vec<super::IntEnum::IntEnum>,
        pub r#shouldContainTwoLongFoos: Vec<super::LongEnum::LongEnum>,
        pub r#stringDefaultsToFoo: String,
        pub r#byteDefaultsToFour: i8,
        pub r#intDefaultsToFive: i32,
        pub r#longDefaultsToNegativeSeven: i64,
        pub r#booleanDefaultsToTrue: bool,
        pub r#charDefaultsToC: u16,
        pub r#floatDefaultsToPi: f32,
        pub r#doubleWithDefault: f64,
        pub r#arrayDefaultsTo123: Vec<i32>,
        pub r#arrayDefaultsToEmpty: Vec<i32>,
        pub r#boolDefault: bool,
        pub r#byteDefault: i8,
        pub r#intDefault: i32,
        pub r#longDefault: i64,
        pub r#floatDefault: f32,
        pub r#doubleDefault: f64,
        pub r#checkDoubleFromFloat: f64,
        pub r#checkStringArray1: Vec<String>,
        pub r#checkStringArray2: Vec<String>,
        pub r#int32_min: i32,
        pub r#int32_max: i32,
        pub r#int64_max: i64,
        pub r#hexInt32_neg_1: i32,
        pub r#ibinder: Option<rsbinder::SIBinder>,
        pub r#empty: Empty::Empty,
        pub r#int8_1: Vec<u8>,
        pub r#int32_1: Vec<i32>,
        pub r#int64_1: Vec<i64>,
        pub r#hexInt32_pos_1: i32,
        pub r#hexInt64_pos_1: i32,
        pub r#const_exprs_1: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub r#const_exprs_2: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub r#const_exprs_3: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub r#const_exprs_4: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub r#const_exprs_5: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub r#const_exprs_6: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub r#const_exprs_7: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub r#const_exprs_8: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub r#const_exprs_9: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub r#const_exprs_10: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub r#addString1: String,
        pub r#addString2: String,
        pub r#shouldSetBit0AndBit2: i32,
        pub r#u: Option<super::Union::Union>,
        pub r#shouldBeConstS1: Option<super::Union::Union>,
        pub r#defaultWithFoo: super::IntEnum::IntEnum,
    }
    impl Default for StructuredParcelable {
        fn default() -> Self {
            Self {
                r#shouldContainThreeFs: Default::default(),
                r#f: Default::default(),
                r#shouldBeJerry: Default::default(),
                r#shouldBeByteBar: Default::default(),
                r#shouldBeIntBar: Default::default(),
                r#shouldBeLongBar: Default::default(),
                r#shouldContainTwoByteFoos: Default::default(),
                r#shouldContainTwoIntFoos: Default::default(),
                r#shouldContainTwoLongFoos: Default::default(),
                r#stringDefaultsToFoo: "foo".into(),
                r#byteDefaultsToFour: 4,
                r#intDefaultsToFive: 5,
                r#longDefaultsToNegativeSeven: -7,
                r#booleanDefaultsToTrue: true,
                r#charDefaultsToC: '\'' as u16,
                r#floatDefaultsToPi: 3.14f32,
                r#doubleWithDefault: -314000000000000000f64,
                r#arrayDefaultsTo123: vec![1,2,3,],
                r#arrayDefaultsToEmpty: Default::default(),
                r#boolDefault: Default::default(),
                r#byteDefault: Default::default(),
                r#intDefault: Default::default(),
                r#longDefault: Default::default(),
                r#floatDefault: Default::default(),
                r#doubleDefault: Default::default(),
                r#checkDoubleFromFloat: 3.14f64,
                r#checkStringArray1: vec!["a".into(),"b".into(),],
                r#checkStringArray2: vec!["a".into(),"b".into(),],
                r#int32_min: -2147483648,
                r#int32_max: 2147483647,
                r#int64_max: 9223372036854775807,
                r#hexInt32_neg_1: -1,
                r#ibinder: Default::default(),
                r#empty: Default::default(),
                r#int8_1: vec![1,1,1,1,1,],
                r#int32_1: vec![1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,],
                r#int64_1: vec![1,1,1,1,1,1,1,1,1,1,],
                r#hexInt32_pos_1: 1,
                r#hexInt64_pos_1: 1,
                r#const_exprs_1: Default::default(),
                r#const_exprs_2: Default::default(),
                r#const_exprs_3: Default::default(),
                r#const_exprs_4: Default::default(),
                r#const_exprs_5: Default::default(),
                r#const_exprs_6: Default::default(),
                r#const_exprs_7: Default::default(),
                r#const_exprs_8: Default::default(),
                r#const_exprs_9: Default::default(),
                r#const_exprs_10: Default::default(),
                r#addString1: "hello world!".into(),
                r#addString2: "The quick brown fox jumps over the lazy dog.".into(),
                r#shouldSetBit0AndBit2: Default::default(),
                r#u: Default::default(),
                r#shouldBeConstS1: Default::default(),
                r#defaultWithFoo: super::IntEnum::IntEnum::FOO,
            }
        }
    }
    impl rsbinder::Parcelable for StructuredParcelable {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                _sub_parcel.write(&self.r#shouldContainThreeFs)?;
                _sub_parcel.write(&self.r#f)?;
                _sub_parcel.write(&self.r#shouldBeJerry)?;
                _sub_parcel.write(&self.r#shouldBeByteBar)?;
                _sub_parcel.write(&self.r#shouldBeIntBar)?;
                _sub_parcel.write(&self.r#shouldBeLongBar)?;
                _sub_parcel.write(&self.r#shouldContainTwoByteFoos)?;
                _sub_parcel.write(&self.r#shouldContainTwoIntFoos)?;
                _sub_parcel.write(&self.r#shouldContainTwoLongFoos)?;
                _sub_parcel.write(&self.r#stringDefaultsToFoo)?;
                _sub_parcel.write(&self.r#byteDefaultsToFour)?;
                _sub_parcel.write(&self.r#intDefaultsToFive)?;
                _sub_parcel.write(&self.r#longDefaultsToNegativeSeven)?;
                _sub_parcel.write(&self.r#booleanDefaultsToTrue)?;
                _sub_parcel.write(&self.r#charDefaultsToC)?;
                _sub_parcel.write(&self.r#floatDefaultsToPi)?;
                _sub_parcel.write(&self.r#doubleWithDefault)?;
                _sub_parcel.write(&self.r#arrayDefaultsTo123)?;
                _sub_parcel.write(&self.r#arrayDefaultsToEmpty)?;
                _sub_parcel.write(&self.r#boolDefault)?;
                _sub_parcel.write(&self.r#byteDefault)?;
                _sub_parcel.write(&self.r#intDefault)?;
                _sub_parcel.write(&self.r#longDefault)?;
                _sub_parcel.write(&self.r#floatDefault)?;
                _sub_parcel.write(&self.r#doubleDefault)?;
                _sub_parcel.write(&self.r#checkDoubleFromFloat)?;
                _sub_parcel.write(&self.r#checkStringArray1)?;
                _sub_parcel.write(&self.r#checkStringArray2)?;
                _sub_parcel.write(&self.r#int32_min)?;
                _sub_parcel.write(&self.r#int32_max)?;
                _sub_parcel.write(&self.r#int64_max)?;
                _sub_parcel.write(&self.r#hexInt32_neg_1)?;
                _sub_parcel.write(&self.r#ibinder)?;
                _sub_parcel.write(&self.r#empty)?;
                _sub_parcel.write(&self.r#int8_1)?;
                _sub_parcel.write(&self.r#int32_1)?;
                _sub_parcel.write(&self.r#int64_1)?;
                _sub_parcel.write(&self.r#hexInt32_pos_1)?;
                _sub_parcel.write(&self.r#hexInt64_pos_1)?;
                _sub_parcel.write(&self.r#const_exprs_1)?;
                _sub_parcel.write(&self.r#const_exprs_2)?;
                _sub_parcel.write(&self.r#const_exprs_3)?;
                _sub_parcel.write(&self.r#const_exprs_4)?;
                _sub_parcel.write(&self.r#const_exprs_5)?;
                _sub_parcel.write(&self.r#const_exprs_6)?;
                _sub_parcel.write(&self.r#const_exprs_7)?;
                _sub_parcel.write(&self.r#const_exprs_8)?;
                _sub_parcel.write(&self.r#const_exprs_9)?;
                _sub_parcel.write(&self.r#const_exprs_10)?;
                _sub_parcel.write(&self.r#addString1)?;
                _sub_parcel.write(&self.r#addString2)?;
                _sub_parcel.write(&self.r#shouldSetBit0AndBit2)?;
                _sub_parcel.write(&self.r#u)?;
                _sub_parcel.write(&self.r#shouldBeConstS1)?;
                _sub_parcel.write(&self.r#defaultWithFoo)?;
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                self.r#shouldContainThreeFs = _sub_parcel.read()?;
                self.r#f = _sub_parcel.read()?;
                self.r#shouldBeJerry = _sub_parcel.read()?;
                self.r#shouldBeByteBar = _sub_parcel.read()?;
                self.r#shouldBeIntBar = _sub_parcel.read()?;
                self.r#shouldBeLongBar = _sub_parcel.read()?;
                self.r#shouldContainTwoByteFoos = _sub_parcel.read()?;
                self.r#shouldContainTwoIntFoos = _sub_parcel.read()?;
                self.r#shouldContainTwoLongFoos = _sub_parcel.read()?;
                self.r#stringDefaultsToFoo = _sub_parcel.read()?;
                self.r#byteDefaultsToFour = _sub_parcel.read()?;
                self.r#intDefaultsToFive = _sub_parcel.read()?;
                self.r#longDefaultsToNegativeSeven = _sub_parcel.read()?;
                self.r#booleanDefaultsToTrue = _sub_parcel.read()?;
                self.r#charDefaultsToC = _sub_parcel.read()?;
                self.r#floatDefaultsToPi = _sub_parcel.read()?;
                self.r#doubleWithDefault = _sub_parcel.read()?;
                self.r#arrayDefaultsTo123 = _sub_parcel.read()?;
                self.r#arrayDefaultsToEmpty = _sub_parcel.read()?;
                self.r#boolDefault = _sub_parcel.read()?;
                self.r#byteDefault = _sub_parcel.read()?;
                self.r#intDefault = _sub_parcel.read()?;
                self.r#longDefault = _sub_parcel.read()?;
                self.r#floatDefault = _sub_parcel.read()?;
                self.r#doubleDefault = _sub_parcel.read()?;
                self.r#checkDoubleFromFloat = _sub_parcel.read()?;
                self.r#checkStringArray1 = _sub_parcel.read()?;
                self.r#checkStringArray2 = _sub_parcel.read()?;
                self.r#int32_min = _sub_parcel.read()?;
                self.r#int32_max = _sub_parcel.read()?;
                self.r#int64_max = _sub_parcel.read()?;
                self.r#hexInt32_neg_1 = _sub_parcel.read()?;
                self.r#ibinder = _sub_parcel.read()?;
                self.r#empty = _sub_parcel.read()?;
                self.r#int8_1 = _sub_parcel.read()?;
                self.r#int32_1 = _sub_parcel.read()?;
                self.r#int64_1 = _sub_parcel.read()?;
                self.r#hexInt32_pos_1 = _sub_parcel.read()?;
                self.r#hexInt64_pos_1 = _sub_parcel.read()?;
                self.r#const_exprs_1 = _sub_parcel.read()?;
                self.r#const_exprs_2 = _sub_parcel.read()?;
                self.r#const_exprs_3 = _sub_parcel.read()?;
                self.r#const_exprs_4 = _sub_parcel.read()?;
                self.r#const_exprs_5 = _sub_parcel.read()?;
                self.r#const_exprs_6 = _sub_parcel.read()?;
                self.r#const_exprs_7 = _sub_parcel.read()?;
                self.r#const_exprs_8 = _sub_parcel.read()?;
                self.r#const_exprs_9 = _sub_parcel.read()?;
                self.r#const_exprs_10 = _sub_parcel.read()?;
                self.r#addString1 = _sub_parcel.read()?;
                self.r#addString2 = _sub_parcel.read()?;
                self.r#shouldSetBit0AndBit2 = _sub_parcel.read()?;
                self.r#u = _sub_parcel.read()?;
                self.r#shouldBeConstS1 = _sub_parcel.read()?;
                self.r#defaultWithFoo = _sub_parcel.read()?;
                Ok(())
            })
        }
    }
    rsbinder::impl_serialize_for_parcelable!(StructuredParcelable);
    rsbinder::impl_deserialize_for_parcelable!(StructuredParcelable);
    impl rsbinder::ParcelableMetadata for StructuredParcelable {
        fn descriptor() -> &'static str { "android.aidl.tests.StructuredParcelable" }
    }
    pub mod Empty {
        #![allow(non_upper_case_globals, non_snake_case, dead_code)]
        #[derive(Debug)]
        #[derive(Clone,PartialEq)]
        pub struct Empty {
        }
        impl Default for Empty {
            fn default() -> Self {
                Self {
                }
            }
        }
        impl rsbinder::Parcelable for Empty {
            fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
                _parcel.sized_write(|_sub_parcel| {
                    Ok(())
                })
            }
            fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
                _parcel.sized_read(|_sub_parcel| {
                    Ok(())
                })
            }
        }
        rsbinder::impl_serialize_for_parcelable!(Empty);
        rsbinder::impl_deserialize_for_parcelable!(Empty);
        impl rsbinder::ParcelableMetadata for Empty {
            fn descriptor() -> &'static str { "android.aidl.tests.StructuredParcelable.Empty" }
        }
    }
}
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
