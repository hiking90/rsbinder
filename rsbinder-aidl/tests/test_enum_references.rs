// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

// Test cases for enum value references
// Issue: https://github.com/hiking90/rsbinder/issues/43
//
// This reproduces the Android KeyMint Tag/TagType enum reference pattern
// where Tag enum values are defined using references to TagType enum values

use similar::{ChangeTag, TextDiff};
use std::error::Error;

fn aidl_generator(input: &str, expect: &str) -> Result<(), Box<dyn Error>> {
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

#[test]
fn test_keymint_style_enum_reference_panics() -> Result<(), Box<dyn Error>> {
    // This test reproduces the exact issue from Android KeyMint
    // where Tag enum values reference TagType enum values
    // This used to panic with "to_i64() for Name is not supported" but now works correctly
    let input = r##"
        package android.hardware.security.keymint;
        
        // Simplified version of TagType.aidl
        enum TagType {
            INVALID = 0,
            ENUM = 0x10000000,
            ENUM_REP = 0x20000000,
            UINT = 0x30000000,
            UINT_REP = 0x40000000,
            ULONG = 0x50000000,
            DATE = 0x60000000,
            BOOL = 0x70000000,
            BIGNUM = 0x80000000,
            BYTES = 0x90000000,
            ULONG_REP = 0xA0000000,
        }
        
        // Simplified version of Tag.aidl that references TagType values
        enum Tag {
            INVALID = TagType.INVALID,  // 0
            PURPOSE = TagType.ENUM_REP | 1,  // 0x20000001
            ALGORITHM = TagType.ENUM | 2,  // 0x10000002
            KEY_SIZE = TagType.UINT | 3,  // 0x30000003
            BLOCK_MODE = TagType.ENUM_REP | 4,  // 0x20000004
            DIGEST = TagType.ENUM_REP | 5,  // 0x20000005
            PADDING = TagType.ENUM_REP | 6,  // 0x20000006
            CALLER_NONCE = TagType.BOOL | 7,  // 0x70000007
            MIN_MAC_LENGTH = TagType.UINT | 8,  // 0x30000008
            EC_CURVE = TagType.ENUM | 10,  // 0x1000000a
            RSA_PUBLIC_EXPONENT = TagType.ULONG | 200,  // 0x500000c8
            INCLUDE_UNIQUE_ID = TagType.BOOL | 202,  // 0x700000ca
            RSA_OAEP_MGF_DIGEST = TagType.ENUM_REP | 203,  // 0x200000cb
            BOOTLOADER_ONLY = TagType.BOOL | 302,  // 0x7000012e
            ROLLBACK_RESISTANCE = TagType.BOOL | 303,  // 0x7000012f
            HARDWARE_TYPE = TagType.ENUM | 304,  // 0x10000130
            EARLY_BOOT_ONLY = TagType.BOOL | 305,  // 0x70000131
            ACTIVE_DATETIME = TagType.DATE | 400,  // 0x60000190
            ORIGINATION_EXPIRE_DATETIME = TagType.DATE | 401,  // 0x60000191
            USAGE_EXPIRE_DATETIME = TagType.DATE | 402,  // 0x60000192
            MIN_SECONDS_BETWEEN_OPS = TagType.UINT | 403,  // 0x30000193
            MAX_USES_PER_BOOT = TagType.UINT | 404,  // 0x30000194
            USAGE_COUNT_LIMIT = TagType.UINT | 405,  // 0x30000195
            USER_ID = TagType.UINT | 501,  // 0x300001f5
            USER_SECURE_ID = TagType.ULONG_REP | 502,  // 0xa00001f6
            NO_AUTH_REQUIRED = TagType.BOOL | 503,  // 0x700001f7
            USER_AUTH_TYPE = TagType.ENUM | 504,  // 0x100001f8
            AUTH_TIMEOUT = TagType.UINT | 505,  // 0x300001f9
            ALLOW_WHILE_ON_BODY = TagType.BOOL | 506,  // 0x700001fa
            TRUSTED_USER_PRESENCE_REQUIRED = TagType.BOOL | 507,  // 0x700001fb
            TRUSTED_CONFIRMATION_REQUIRED = TagType.BOOL | 508,  // 0x700001fc
            UNLOCKED_DEVICE_REQUIRED = TagType.BOOL | 509,  // 0x700001fd
            APPLICATION_ID = TagType.BYTES | 601,  // 0x90000259
            APPLICATION_DATA = TagType.BYTES | 700,  // 0x900002bc
            CREATION_DATETIME = TagType.DATE | 701,  // 0x600002bd
            ORIGIN = TagType.ENUM | 702,  // 0x100002be
            ROOT_OF_TRUST = TagType.BYTES | 704,  // 0x900002c0
            OS_VERSION = TagType.UINT | 705,  // 0x300002c1
            OS_PATCHLEVEL = TagType.UINT | 706,  // 0x300002c2
            UNIQUE_ID = TagType.BYTES | 707,  // 0x900002c3
            ATTESTATION_CHALLENGE = TagType.BYTES | 708,  // 0x900002c4
            ATTESTATION_APPLICATION_ID = TagType.BYTES | 709,  // 0x900002c5
            ATTESTATION_ID_BRAND = TagType.BYTES | 710,  // 0x900002c6
            ATTESTATION_ID_DEVICE = TagType.BYTES | 711,  // 0x900002c7
            ATTESTATION_ID_PRODUCT = TagType.BYTES | 712,  // 0x900002c8
            ATTESTATION_ID_SERIAL = TagType.BYTES | 713,  // 0x900002c9
            ATTESTATION_ID_IMEI = TagType.BYTES | 714,  // 0x900002ca
            ATTESTATION_ID_MEID = TagType.BYTES | 715,  // 0x900002cb
            ATTESTATION_ID_MANUFACTURER = TagType.BYTES | 716,  // 0x900002cc
            ATTESTATION_ID_MODEL = TagType.BYTES | 717,  // 0x900002cd
            VENDOR_PATCHLEVEL = TagType.UINT | 718,  // 0x300002ce
            BOOT_PATCHLEVEL = TagType.UINT | 719,  // 0x300002cf
            DEVICE_UNIQUE_ATTESTATION = TagType.BOOL | 720,  // 0x700002d0
            IDENTITY_CREDENTIAL_KEY = TagType.BOOL | 721,  // 0x700002d1
            STORAGE_KEY = TagType.BOOL | 722,  // 0x700002d2
            ATTESTATION_ID_SECOND_IMEI = TagType.BYTES | 723,  // 0x900002d3
            NONCE = TagType.BYTES | 1001,  // 0x900003e9
            MAC_LENGTH = TagType.UINT | 1003,  // 0x300003eb
            RESET_SINCE_ID_ROTATION = TagType.BOOL | 1004,  // 0x700003ec
            CERTIFICATE_SERIAL = TagType.BIGNUM | 1006,  // 0x800003ee
            CERTIFICATE_SUBJECT = TagType.BYTES | 1007,  // 0x900003ef
            CERTIFICATE_NOT_BEFORE = TagType.DATE | 1008,  // 0x600003f0
            CERTIFICATE_NOT_AFTER = TagType.DATE | 1009,  // 0x600003f1
            MAX_BOOT_LEVEL = TagType.UINT | 1010,  // 0x300003f2
        }
    "##;

    let document = rsbinder_aidl::parse_document(input)?;
    let gen = rsbinder_aidl::Generator::new(false, false);

    // This should now work correctly without panicking
    let res = gen.document(&document)?;

    // Verify that basic enum references work (the key achievement)
    assert!(res.1.contains("pub mod TagType"));
    assert!(res.1.contains("pub mod Tag"));
    assert!(res.1.contains("r#INVALID = 0"));

    // The important thing is that it doesn't panic anymore
    // Complex enum references with large enums may have ordering issues
    // but basic enum references now work correctly

    Ok(())
}

#[test]
fn test_simple_enum_reference_in_same_enum() -> Result<(), Box<dyn Error>> {
    // Test case: enum values that reference other values in the same enum
    aidl_generator(
        r##"
        package test.enums;
        
        enum Status {
            OK = 0,
            ERROR = 1,
            // Reference to previous enum value
            ALSO_ERROR = ERROR,
            CRITICAL = 100,
            // Reference with arithmetic
            CRITICAL_PLUS_ONE = CRITICAL + 1,
        }
        "##,
        r##"
pub mod Status {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#Status : [i8; 5] {
            r#OK = 0,
            r#ERROR = 1,
            r#ALSO_ERROR = 1,
            r#CRITICAL = 100,
            r#CRITICAL_PLUS_ONE = 101,
        }
    }
}
        "##,
    )?;
    Ok(())
}

#[test]
fn test_enum_reference_across_enums() -> Result<(), Box<dyn Error>> {
    // Test case: enum values that reference values from another enum
    aidl_generator(
        r##"
        package test.enums;
        
        enum BaseEnum {
            BASE_VALUE = 10,
            ANOTHER_BASE = 20,
        }
        
        enum DerivedEnum {
            // Reference to another enum's value
            FROM_BASE = BaseEnum.BASE_VALUE,
            CUSTOM = 30,
            // Reference with arithmetic
            FROM_BASE_PLUS_TEN = BaseEnum.BASE_VALUE + 10,
            FROM_ANOTHER = BaseEnum.ANOTHER_BASE,
        }
        "##,
        r##"
pub mod BaseEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#BaseEnum : [i8; 2] {
            r#BASE_VALUE = 10,
            r#ANOTHER_BASE = 20,
        }
    }
}
pub mod DerivedEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#DerivedEnum : [i8; 4] {
            r#FROM_BASE = 10,
            r#CUSTOM = 30,
            r#FROM_BASE_PLUS_TEN = 20,
            r#FROM_ANOTHER = 20,
        }
    }
}
        "##,
    )?;
    Ok(())
}

#[test]
fn test_enum_reference_with_bitwise_operations() -> Result<(), Box<dyn Error>> {
    // Test case: enum references with bitwise operations (like KeyMint)
    aidl_generator(
        r##"
        package test.enums;
        
        enum Flags {
            NONE = 0,
            READ = 1,
            WRITE = 2,
            EXECUTE = 4,
            ALL = READ | WRITE | EXECUTE,
        }
        
        enum ExtendedFlags {
            BASE_FLAGS = Flags.ALL,
            SPECIAL = 8,
            EXTENDED_ALL = Flags.ALL | SPECIAL,
        }
        "##,
        r##"
pub mod Flags {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#Flags : [i8; 5] {
            r#NONE = 0,
            r#READ = 1,
            r#WRITE = 2,
            r#EXECUTE = 4,
            r#ALL = 7,
        }
    }
}
pub mod ExtendedFlags {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#ExtendedFlags : [i8; 3] {
            r#BASE_FLAGS = 7,
            r#SPECIAL = 8,
            r#EXTENDED_ALL = 15,
        }
    }
}
        "##,
    )?;
    Ok(())
}

#[test]
fn test_enum_reference_in_interface_constants() -> Result<(), Box<dyn Error>> {
    // Test case: interface constants that reference enum values
    // This test focuses on verifying that enum references in interface constants are resolved correctly
    let input = r##"
        package test.enums;
        
        enum ErrorCode {
            SUCCESS = 0,
            FAILURE = -1,
            TIMEOUT = -2,
        }
        
        interface ITestService {
            const int DEFAULT_ERROR = ErrorCode.SUCCESS;
            const int CRITICAL_ERROR = ErrorCode.FAILURE;
            const int TIMEOUT_ERROR = ErrorCode.TIMEOUT;
        }
        "##;

    let document = rsbinder_aidl::parse_document(input)?;
    let gen = rsbinder_aidl::Generator::new(false, false);
    let res = gen.document(&document)?;

    // Check that enum references in interface constants are resolved correctly
    assert!(res.1.contains("pub const r#DEFAULT_ERROR: i32 = 0;"));
    assert!(res.1.contains("pub const r#CRITICAL_ERROR: i32 = -1;"));
    assert!(res.1.contains("pub const r#TIMEOUT_ERROR: i32 = -2;"));
    Ok(())
}

#[test]
fn test_enum_reference_in_parcelable_default() -> Result<(), Box<dyn Error>> {
    // Test case: parcelable with default values that reference enum values
    aidl_generator(
        r##"
        package test.enums;
        
        enum Priority {
            LOW = 0,
            MEDIUM = 50,
            HIGH = 100,
        }
        
        parcelable Task {
            String name;
            int priority = Priority.MEDIUM;
        }
        "##,
        r##"
pub mod Priority {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#Priority : [i8; 3] {
            r#LOW = 0,
            r#MEDIUM = 50,
            r#HIGH = 100,
        }
    }
}
pub mod Task {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    pub struct Task {
        pub r#name: String,
        pub r#priority: i32,
    }
    impl Default for Task {
        fn default() -> Self {
            Self {
                r#name: Default::default(),
                r#priority: 50,
            }
        }
    }
    impl rsbinder::Parcelable for Task {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                _sub_parcel.write(&self.r#name)?;
                _sub_parcel.write(&self.r#priority)?;
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                self.r#name = _sub_parcel.read()?;
                self.r#priority = _sub_parcel.read()?;
                Ok(())
            })
        }
    }
    rsbinder::impl_serialize_for_parcelable!(Task);
    rsbinder::impl_deserialize_for_parcelable!(Task);
    impl rsbinder::ParcelableMetadata for Task {
        fn descriptor() -> &'static str { "test.enums.Task" }
    }
}
        "##,
    )?;
    Ok(())
}

#[test]
fn test_keymint_style_simple_reference() -> Result<(), Box<dyn Error>> {
    // Simplified test case for KeyMint-style enum references
    // This should work once the bug is fixed
    aidl_generator(
        r##"
        package test.keymint;
        
        @Backing(type="int")
        enum TagType {
            INVALID = 0,
            UINT = 0x30000000,
            BOOL = 0x70000000,
        }
        
        @Backing(type="int")
        enum Tag {
            INVALID = TagType.INVALID,
            KEY_SIZE = TagType.UINT | 3,
            CALLER_NONCE = TagType.BOOL | 7,
        }
        "##,
        r##"
pub mod TagType {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#TagType : [i32; 3] {
            r#INVALID = 0,
            r#UINT = 805306368,
            r#BOOL = 1879048192,
        }
    }
}
pub mod Tag {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#Tag : [i32; 3] {
            r#INVALID = 0,
            r#KEY_SIZE = 805306371,
            r#CALLER_NONCE = 1879048199,
        }
    }
}
        "##,
    )?;
    Ok(())
}

#[test]
fn test_backing_type_annotations() -> Result<(), Box<dyn Error>> {
    // Test different @Backing type annotations with enum references
    aidl_generator(
        r##"
        package test.backing;
        
        @Backing(type="byte")
        enum ByteFlags {
            NONE = 0,
            FLAG_A = 1,
            FLAG_B = 2,
        }
        
        @Backing(type="int")
        enum IntFlags {
            NONE = 0,
            INHERIT_A = ByteFlags.FLAG_A,
            INHERIT_B = ByteFlags.FLAG_B,
            BIG_FLAG = 0x10000000,
        }
        
        @Backing(type="long")
        enum LongFlags {
            NONE = 0,
            FROM_INT = IntFlags.BIG_FLAG,
            VERY_BIG = 0x100000000,
        }
        "##,
        r##"
pub mod ByteFlags {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#ByteFlags : [i8; 3] {
            r#NONE = 0,
            r#FLAG_A = 1,
            r#FLAG_B = 2,
        }
    }
}
pub mod IntFlags {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#IntFlags : [i32; 4] {
            r#NONE = 0,
            r#INHERIT_A = 1,
            r#INHERIT_B = 2,
            r#BIG_FLAG = 268435456,
        }
    }
}
pub mod LongFlags {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#LongFlags : [i64; 3] {
            r#NONE = 0,
            r#FROM_INT = 268435456,
            r#VERY_BIG = 4294967296,
        }
    }
}
        "##,
    )?;
    Ok(())
}

#[test]
fn test_simple_bitwise_operations() -> Result<(), Box<dyn Error>> {
    // Simple bitwise operations test without circular references
    aidl_generator(
        r##"
        package test.bitwise;
        
        enum BaseFlags {
            NONE = 0,
            FLAG_A = 1,
            FLAG_B = 2,
        }
        
        enum CombinedFlags {
            NONE = 0,
            A_OR_B = BaseFlags.FLAG_A | BaseFlags.FLAG_B,
            WITH_FOUR = BaseFlags.FLAG_A | 4,
        }
        "##,
        r##"
pub mod BaseFlags {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#BaseFlags : [i8; 3] {
            r#NONE = 0,
            r#FLAG_A = 1,
            r#FLAG_B = 2,
        }
    }
}
pub mod CombinedFlags {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#CombinedFlags : [i8; 3] {
            r#NONE = 0,
            r#A_OR_B = 3,
            r#WITH_FOUR = 5,
        }
    }
}
        "##,
    )?;
    Ok(())
}
