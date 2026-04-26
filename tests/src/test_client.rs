#![allow(non_snake_case, dead_code, unused_imports, unused_macros)]

use env_logger::Env;

pub use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/test_aidl.rs"));

use android::aidl::fixedsizearray::FixedSizeArrayExample::{
    FixedSizeArrayExample,
    IRepeatFixedSizeArray::{BpRepeatFixedSizeArray, IRepeatFixedSizeArray},
    IntParcelable::IntParcelable,
};
use android::aidl::tests::nested::{INestedService, ParcelableWithNested};
use android::aidl::tests::nonvintf::{
    NonVintfExtendableParcelable::NonVintfExtendableParcelable,
    NonVintfParcelable::NonVintfParcelable,
};
use android::aidl::tests::unions::EnumUnion::EnumUnion;
use android::aidl::tests::unstable::{
    UnstableExtendableParcelable::UnstableExtendableParcelable,
    UnstableParcelable::UnstableParcelable,
};
use android::aidl::tests::vintf::{
    VintfExtendableParcelable::VintfExtendableParcelable, VintfParcelable::VintfParcelable,
};
use android::aidl::tests::INamedCallback;
use android::aidl::tests::INewName::{self, BpNewName};
use android::aidl::tests::IOldName::{self, BpOldName};
use android::aidl::tests::ITestService::{
    self, BpTestService, Empty::Empty, ITestServiceDefault, ITestServiceDefaultRef,
};
use android::aidl::tests::{
    extension::ExtendableParcelable::ExtendableParcelable, extension::MyExt::MyExt,
    extension::MyExt2::MyExt2, extension::MyExtLike::MyExtLike, BackendType::BackendType,
    ByteEnum::ByteEnum, IntEnum::IntEnum, LongEnum::LongEnum, RecursiveList::RecursiveList,
    StructuredParcelable, Union,
};
use android::aidl::versioned::tests::{
    BazUnion::BazUnion, Foo::Foo, IFooInterface, IFooInterface::BpFooInterface,
};
use rustix::fd::OwnedFd;
use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::sync::{Arc, Mutex};
use std::{
    fs::File,
    os::fd::{AsRawFd, IntoRawFd},
};

fn init_logger() {
    let _ = env_logger::Builder::from_env(Env::default().default_filter_or("debug")).try_init();
}

fn init_test() {
    init_logger();
    ProcessState::init_default();
    ProcessState::start_thread_pool();
}

fn get_test_service() -> rsbinder::Strong<dyn ITestService::ITestService> {
    init_test();
    hub::get_interface(<BpTestService as ITestService::ITestService>::descriptor()).unwrap_or_else(
        |_| {
            panic!(
                "did not get binder service: {}",
                <BpTestService as ITestService::ITestService>::descriptor()
            )
        },
    )
}

#[test]
fn test_constants() {
    assert_eq!(ITestService::A1, 1);
    assert_eq!(ITestService::A2, 1);
    assert_eq!(ITestService::A3, 1);
    assert_eq!(ITestService::A4, 1);
    assert_eq!(ITestService::A5, 1);
    assert_eq!(ITestService::A6, 1);
    assert_eq!(ITestService::A7, 1);
    assert_eq!(ITestService::A8, 1);
    assert_eq!(ITestService::A9, 1);
    assert_eq!(ITestService::A10, 1);
    assert_eq!(ITestService::A11, 1);
    assert_eq!(ITestService::A12, 1);
    assert_eq!(ITestService::A13, 1);
    assert_eq!(ITestService::A14, 1);
    assert_eq!(ITestService::A15, 1);
    assert_eq!(ITestService::A16, 1);
    assert_eq!(ITestService::A17, 1);
    assert_eq!(ITestService::A18, 1);
    assert_eq!(ITestService::A19, 1);
    assert_eq!(ITestService::A20, 1);
    assert_eq!(ITestService::A21, 1);
    assert_eq!(ITestService::A22, 1);
    assert_eq!(ITestService::A23, 1);
    assert_eq!(ITestService::A24, 1);
    assert_eq!(ITestService::A25, 1);
    assert_eq!(ITestService::A26, 1);
    assert_eq!(ITestService::A27, 1);
    assert_eq!(ITestService::A28, 1);
    assert_eq!(ITestService::A29, 1);
    assert_eq!(ITestService::A30, 1);
    assert_eq!(ITestService::A31, 1);
    assert_eq!(ITestService::A32, 1);
    assert_eq!(ITestService::A33, 1);
    assert_eq!(ITestService::A34, 1);
    assert_eq!(ITestService::A35, 1);
    assert_eq!(ITestService::A36, 1);
    assert_eq!(ITestService::A37, 1);
    assert_eq!(ITestService::A38, 1);
    assert_eq!(ITestService::A39, 1);
    assert_eq!(ITestService::A40, 1);
    assert_eq!(ITestService::A41, 1);
    assert_eq!(ITestService::A42, 1);
    assert_eq!(ITestService::A43, 1);
    assert_eq!(ITestService::A44, 1);
    assert_eq!(ITestService::A45, 1);
    assert_eq!(ITestService::A46, 1);
    assert_eq!(ITestService::A47, 1);
    assert_eq!(ITestService::A48, 1);
    assert_eq!(ITestService::A49, 1);
    assert_eq!(ITestService::A50, 1);
    assert_eq!(ITestService::A51, 1);
    assert_eq!(ITestService::A52, 1);
    assert_eq!(ITestService::A53, 1);
    assert_eq!(ITestService::A54, 1);
    assert_eq!(ITestService::A55, 1);
    assert_eq!(ITestService::A56, 1);
    assert_eq!(ITestService::A57, 1);
}

#[test]
fn test_oneway() {
    let result = get_test_service().TestOneway();
    assert_eq!(result, Ok(()));
}

macro_rules! test_primitive {
    ($test:ident, $func:ident, $value:expr) => {
        #[test]
        fn $test() {
            let value = $value;
            let result = get_test_service().$func(value);
            assert_eq!(result, Ok(value));
        }
    };
}

test_primitive! {test_primitive_bool_false, RepeatBoolean, false}
test_primitive! {test_primitive_bool_true, RepeatBoolean, true}
test_primitive! {test_primitive_byte, RepeatByte, -128i8}
test_primitive! {test_primitive_char, RepeatChar, 'A' as u16}
test_primitive! {test_primitive_int, RepeatInt, 1i32 << 30}
test_primitive! {test_primitive_long, RepeatLong, 1i64 << 60}
test_primitive! {test_primitive_float, RepeatFloat, 1.0f32 / 3.0f32}
test_primitive! {test_primitive_double, RepeatDouble, 1.0f64 / 3.0f64}
test_primitive! {test_primitive_byte_constant, RepeatByte, ITestService::BYTE_TEST_CONSTANT}
test_primitive! {test_primitive_constant1, RepeatInt, ITestService::TEST_CONSTANT}
test_primitive! {test_primitive_constant2, RepeatInt, ITestService::TEST_CONSTANT2}
test_primitive! {test_primitive_constant3, RepeatInt, ITestService::TEST_CONSTANT3}
test_primitive! {test_primitive_constant4, RepeatInt, ITestService::TEST_CONSTANT4}
test_primitive! {test_primitive_constant5, RepeatInt, ITestService::TEST_CONSTANT5}
test_primitive! {test_primitive_constant6, RepeatInt, ITestService::TEST_CONSTANT6}
test_primitive! {test_primitive_constant7, RepeatInt, ITestService::TEST_CONSTANT7}
test_primitive! {test_primitive_constant8, RepeatInt, ITestService::TEST_CONSTANT8}
test_primitive! {test_primitive_constant9, RepeatInt, ITestService::TEST_CONSTANT9}
test_primitive! {test_primitive_constant10, RepeatInt, ITestService::TEST_CONSTANT10}
test_primitive! {test_primitive_constant11, RepeatInt, ITestService::TEST_CONSTANT11}
test_primitive! {test_primitive_constant12, RepeatInt, ITestService::TEST_CONSTANT12}
test_primitive! {test_primitive_long_constant, RepeatLong, ITestService::LONG_TEST_CONSTANT}
test_primitive! {test_primitive_byte_enum, RepeatByteEnum, ByteEnum::FOO}
test_primitive! {test_primitive_int_enum, RepeatIntEnum, IntEnum::BAR}
test_primitive! {test_primitive_long_enum, RepeatLongEnum, LongEnum::FOO}

#[test]
fn test_repeat_string() {
    let service = get_test_service();
    let inputs = [
        "typical string".into(),
        String::new(),
        "\0\0".into(),
        // This is actually two unicode code points:
        //   U+10437: The 'small letter yee' character in the deseret alphabet
        //   U+20AC: A euro sign
        String::from_utf16(&[0xD801, 0xDC37, 0x20AC]).expect("error converting string"),
        ITestService::STRING_TEST_CONSTANT.into(),
        ITestService::STRING_TEST_CONSTANT2.into(),
    ];
    for input in &inputs {
        let result = service.RepeatString(input);
        assert_eq!(result.as_ref(), Ok(input));
    }
}

macro_rules! test_reverse_array {
    ($test:ident, $func:ident, $array:expr) => {
        #[test]
        fn $test() {
            let mut array = $array.to_vec();

            // Java needs initial values here (can't resize arrays)
            let mut repeated = vec![Default::default(); array.len()];

            let result = get_test_service().$func(&array, &mut repeated);
            assert_eq!(repeated, array);
            array.reverse();
            assert_eq!(result, Ok(array));
        }
    };
}

test_reverse_array! {test_array_boolean, ReverseBoolean, [true, false, false]}
test_reverse_array! {test_array_byte, ReverseByte, [255u8, 0u8, 127u8]}
test_reverse_array! {
    service,
    ReverseChar,
    ['A' as u16, 'B' as u16, 'C' as u16]
}
test_reverse_array! {test_array_int, ReverseInt, [1, 2, 3]}
test_reverse_array! {test_array_long, ReverseLong, [-1i64, 0i64, 1i64 << 60]}
test_reverse_array! {test_array_float, ReverseFloat, [-0.3f32, -0.7f32, 8.0f32]}
test_reverse_array! {
    test_array_double,
    ReverseDouble,
    [1.0f64 / 3.0f64, 1.0f64 / 7.0f64, 42.0f64]
}
test_reverse_array! {
    test_array_string,
    ReverseString,
    ["f".into(), "a".into(), "b".into()]
}
test_reverse_array! {
    test_array_byte_enum,
    ReverseByteEnum,
    [ByteEnum::FOO, ByteEnum::BAR, ByteEnum::BAR]
}
test_reverse_array! {
    test_array_byte_enum_values,
    ReverseByteEnum,
    ByteEnum::enum_values()
}
test_reverse_array! {
    test_array_byte_enum_v2,
    ReverseByteEnum,
    [ByteEnum::FOO, ByteEnum::BAR, ByteEnum::BAZ]
}
test_reverse_array! {
    test_array_int_enum,
    ReverseIntEnum,
    [IntEnum::FOO, IntEnum::BAR, IntEnum::BAR]
}
test_reverse_array! {
    test_array_long_enum,
    ReverseLongEnum,
    [LongEnum::FOO, LongEnum::BAR, LongEnum::BAR]
}
test_reverse_array! {
    test_array_string_list,
    ReverseStringList,
    ["f".into(), "a".into(), "b".into()]
}
test_reverse_array! {
    test_array_utf8_string,
    ReverseUtf8CppString,
    ["a".into(),
        String::new(),
        std::str::from_utf8(&[0xC3, 0xB8])
            .expect("error converting string")
            .into()]
}

#[test]
fn test_binder_exchange() {
    const NAME: &str = "Smythe";
    let service = get_test_service();
    let got = service
        .GetOtherTestService(NAME)
        .expect("error calling GetOtherTestService");
    assert_eq!(got.GetName().as_ref().map(String::as_ref), Ok(NAME));
    assert_eq!(service.VerifyName(&got, NAME), Ok(true));
}

#[test]
fn test_binder_array_exchange() {
    let names = vec!["Fizz".into(), "Buzz".into()];
    let service = get_test_service();
    let got = service
        .GetInterfaceArray(&names)
        .expect("error calling GetInterfaceArray");
    assert_eq!(
        got.iter()
            .map(|s| s.GetName())
            .collect::<std::result::Result<Vec<_>, _>>(),
        Ok(names.clone())
    );
    assert_eq!(
        service.VerifyNamesWithInterfaceArray(&got, &names),
        Ok(true)
    );
}

#[test]
fn test_binder_nullable_array_exchange() {
    let names = vec![Some("Fizz".into()), None, Some("Buzz".into())];
    let service = get_test_service();
    let got = service
        .GetNullableInterfaceArray(Some(&names))
        .expect("error calling GetNullableInterfaceArray");
    assert_eq!(
        got.as_ref().map(|arr| arr
            .iter()
            .map(|opt_s| opt_s
                .as_ref()
                .map(|s| s.GetName().expect("error calling GetName")))
            .collect::<Vec<_>>()),
        Some(names.clone())
    );
    assert_eq!(
        service.VerifyNamesWithNullableInterfaceArray(got.as_ref().map(|v| &v[..]), Some(&names)),
        Ok(true)
    );
}

#[test]
fn test_interface_list_exchange() {
    let names = vec![Some("Fizz".into()), None, Some("Buzz".into())];
    let service = get_test_service();
    let got = service
        .GetInterfaceList(Some(&names))
        .expect("error calling GetInterfaceList");
    assert_eq!(
        got.as_ref().map(|arr| arr
            .iter()
            .map(|opt_s| opt_s
                .as_ref()
                .map(|s| s.GetName().expect("error calling GetName")))
            .collect::<Vec<_>>()),
        Some(names.clone())
    );
    assert_eq!(
        service.VerifyNamesWithInterfaceList(got.as_ref().map(|v| &v[..]), Some(&names)),
        Ok(true)
    );
}

fn build_pipe() -> (File, File) {
    let fds = rustix::pipe::pipe().expect("error creating pipe");
    // Safety: we get two file descriptors from pipe()
    // and pass them after checking if the function returned
    // without an error, so the descriptors should be valid
    // by that point
    unsafe {
        (
            File::from_raw_fd(fds.0.into_raw_fd()),
            File::from_raw_fd(fds.1.into_raw_fd()),
        )
    }
}

/// Helper function that constructs a `File` from a `ParcelFileDescriptor`.
///
/// This is needed because `File` is currently the way to read and write
/// to pipes using the `Read` and `Write` traits.
fn file_from_pfd(fd: &rsbinder::ParcelFileDescriptor) -> File {
    fd.as_ref()
        .try_clone()
        .expect("failed to clone file descriptor")
        .into()
}

#[test]
fn test_parcel_file_descriptor() {
    let service = get_test_service();
    let (mut read_file, write_file) = build_pipe();

    let write_pfd = rsbinder::ParcelFileDescriptor::new(write_file);
    let result_pfd = service
        .RepeatParcelFileDescriptor(&write_pfd)
        .expect("error calling RepeatParcelFileDescriptor");

    const TEST_DATA: &[u8] = b"FrazzleSnazzleFlimFlamFlibbityGumboChops";
    file_from_pfd(&result_pfd)
        .write_all(TEST_DATA)
        .expect("error writing to pipe");

    let mut buf = [0u8; TEST_DATA.len()];
    read_file
        .read_exact(&mut buf)
        .expect("error reading from pipe");
    assert_eq!(&buf[..], TEST_DATA);
}

#[test]
fn test_parcel_file_descriptor_array() {
    let service = get_test_service();

    let (read_file, write_file) = build_pipe();
    let input = [
        rsbinder::ParcelFileDescriptor::new(read_file),
        rsbinder::ParcelFileDescriptor::new(write_file),
    ];

    let mut repeated = vec![];

    let backend = service
        .getBackendType()
        .expect("error getting backend type");
    if backend == BackendType::JAVA {
        // Java needs initial values here (can't resize arrays)
        // Other backends can't accept 'None', but we can use it in Java for convenience, rather
        // than creating file descriptors.
        repeated = vec![None, None];
    }

    let result = service
        .ReverseParcelFileDescriptorArray(&input[..], &mut repeated)
        .expect("error calling ReverseParcelFileDescriptorArray");

    file_from_pfd(&input[1])
        .write_all(b"First")
        .expect("error writing to pipe");
    file_from_pfd(
        repeated[1]
            .as_ref()
            .expect("received None for ParcelFileDescriptor"),
    )
    .write_all(b"Second")
    .expect("error writing to pipe");
    file_from_pfd(&result[0])
        .write_all(b"Third")
        .expect("error writing to pipe");

    const TEST_DATA: &[u8] = b"FirstSecondThird";
    let mut buf = [0u8; TEST_DATA.len()];
    file_from_pfd(&input[0])
        .read_exact(&mut buf)
        .expect("error reading from pipe");
    assert_eq!(&buf[..], TEST_DATA);
}

#[test]
fn test_service_specific_exception() {
    let service = get_test_service();

    for i in -1..2 {
        let result: std::prelude::v1::Result<(), Status> = service.ThrowServiceException(i);
        assert!(result.is_err());

        let status = result.unwrap_err();
        assert_eq!(
            status.exception_code(),
            rsbinder::ExceptionCode::ServiceSpecific
        );
        assert_eq!(status.service_specific_error(), i);
    }
}

macro_rules! test_nullable {
    ($test:ident, $func:ident, $value:expr) => {
        #[test]
        fn $test() {
            let service = get_test_service();
            let value = Some($value);
            let result = service.$func(value.as_deref());
            assert_eq!(result, Ok(value));

            let result = service.$func(None);
            assert_eq!(result, Ok(None));
        }
    };
}

test_nullable! {test_nullable_array_int, RepeatNullableIntArray, vec![1, 2, 3]}
test_nullable! {
    test_nullable_array_byte_enum,
    RepeatNullableByteEnumArray,
    vec![ByteEnum::FOO, ByteEnum::BAR]
}
test_nullable! {
    test_nullable_array_int_enum,
    RepeatNullableIntEnumArray,
    vec![IntEnum::FOO, IntEnum::BAR]
}
test_nullable! {
    test_nullable_array_long_enum,
    RepeatNullableLongEnumArray,
    vec![LongEnum::FOO, LongEnum::BAR]
}
test_nullable! {test_nullable_string, RepeatNullableString, "Blooob".into()}
test_nullable! {
    test_nullable_string_list,
    RepeatNullableStringList,
    vec![
        Some("Wat".into()),
        Some("Blooob".into()),
        Some("Wat".into()),
        None,
        Some("YEAH".into()),
        Some("OKAAAAY".into()),
    ]
}

#[test]
fn test_nullable_parcelable() {
    let value = Empty {};

    let service = get_test_service();
    let value = Some(value);
    let result = service.RepeatNullableParcelable(value.as_ref());
    assert_eq!(result, Ok(value));

    let result = service.RepeatNullableParcelable(None);
    assert_eq!(result, Ok(None));
}

test_nullable! {
    test_nullable_parcelable_array,
    RepeatNullableParcelableArray,
    vec![
        Some(Empty {}),
        None,
    ]
}

test_nullable! {
    test_nullable_parcelable_list,
    RepeatNullableParcelableList,
    vec![
        Some(Empty {}),
        None,
    ]
}

#[test]
fn test_binder() {
    let service = get_test_service();
    assert!(service
        .GetCallback(true)
        .expect("error calling GetCallback")
        .is_none());
    let callback = service
        .GetCallback(false)
        .expect("error calling GetCallback")
        .expect("expected Some from GetCallback");

    // We don't have any place to get a fresh `SpIBinder`, so we
    // reuse the interface for the binder tests
    let binder = callback.as_binder();
    assert_eq!(service.TakesAnIBinder(&binder), Ok(()));
    assert_eq!(service.TakesANullableIBinder(None), Ok(()));
    assert_eq!(service.TakesANullableIBinder(Some(&binder)), Ok(()));
}

macro_rules! test_reverse_null_array {
    ($service:expr, $func:ident, $expect_repeated:expr) => {{
        let mut repeated = None;
        let result = $service.$func(None, &mut repeated);
        assert_eq!(repeated, $expect_repeated);
        assert_eq!(result, Ok(None));
    }};
}

macro_rules! test_reverse_nullable_array {
    ($service:expr, $func:ident, $array:expr) => {{
        let mut array = $array;
        // Java needs initial values here (can't resize arrays)
        let mut repeated = Some(vec![Default::default(); array.len()]);
        let result = $service.$func(Some(&array[..]), &mut repeated);
        assert_eq!(repeated.as_ref(), Some(&array));
        array.reverse();
        assert_eq!(result, Ok(Some(array)));
    }};
}

#[test]
fn test_utf8_string() {
    let service = get_test_service();
    let inputs = [
        "typical string",
        "",
        "\0\0",
        std::str::from_utf8(&[0xF0, 0x90, 0x90, 0xB7, 0xE2, 0x82, 0xAC])
            .expect("error converting string"),
        ITestService::STRING_TEST_CONSTANT_UTF8,
    ];
    for input in &inputs {
        let result = service.RepeatUtf8CppString(input);
        assert_eq!(result.as_ref().map(String::as_str), Ok(*input));

        let result = service.RepeatNullableUtf8CppString(Some(input));
        assert_eq!(result.as_ref().map(Option::as_deref), Ok(Some(*input)));
    }

    let result = service.RepeatNullableUtf8CppString(None);
    assert_eq!(result, Ok(None));

    let inputs = vec![
        Some("typical string".into()),
        Some(String::new()),
        None,
        Some(
            std::str::from_utf8(&[0xF0, 0x90, 0x90, 0xB7, 0xE2, 0x82, 0xAC])
                .expect("error converting string")
                .into(),
        ),
        Some(ITestService::STRING_TEST_CONSTANT_UTF8.into()),
    ];

    // Java can't return a null list as a parameter
    let backend = service
        .getBackendType()
        .expect("error getting backend type");
    let null_output: Option<Vec<Option<String>>> = if backend == BackendType::JAVA {
        Some(vec![])
    } else {
        None
    };
    test_reverse_null_array!(service, ReverseUtf8CppStringList, null_output);

    test_reverse_null_array!(service, ReverseNullableUtf8CppString, None);

    test_reverse_nullable_array!(service, ReverseUtf8CppStringList, inputs.clone());
    test_reverse_nullable_array!(service, ReverseNullableUtf8CppString, inputs);
}

#[allow(clippy::approx_constant)]
#[allow(clippy::float_cmp)]
#[test]
fn test_parcelable() {
    let service = get_test_service();
    let mut parcelable = StructuredParcelable::StructuredParcelable::default();

    const DESIRED_VALUE: i32 = 23;
    parcelable.f = DESIRED_VALUE;

    assert_eq!(parcelable.stringDefaultsToFoo, "foo");
    assert_eq!(parcelable.byteDefaultsToFour, 4);
    assert_eq!(parcelable.intDefaultsToFive, 5);
    assert_eq!(parcelable.longDefaultsToNegativeSeven, -7);
    assert!(parcelable.booleanDefaultsToTrue);
    assert_eq!(parcelable.charDefaultsToC, 'C' as u16);
    assert_eq!(parcelable.floatDefaultsToPi, 3.14f32);
    assert_eq!(parcelable.doubleWithDefault, -3.14e17f64);
    assert!(!parcelable.boolDefault);
    assert_eq!(parcelable.byteDefault, 0);
    assert_eq!(parcelable.intDefault, 0);
    assert_eq!(parcelable.longDefault, 0);
    assert_eq!(parcelable.floatDefault, 0.0f32);
    assert_eq!(parcelable.doubleDefault, 0.0f64);
    assert_eq!(parcelable.arrayDefaultsTo123, &[1, 2, 3]);
    assert!(parcelable.arrayDefaultsToEmpty.is_empty());

    let result = service.FillOutStructuredParcelable(&mut parcelable);
    assert_eq!(result, Ok(()));

    assert_eq!(
        parcelable.shouldContainThreeFs,
        [DESIRED_VALUE, DESIRED_VALUE, DESIRED_VALUE]
    );
    assert_eq!(parcelable.shouldBeJerry, "Jerry");
    assert_eq!(parcelable.int32_min, i32::MIN);
    assert_eq!(parcelable.int32_max, i32::MAX);
    assert_eq!(parcelable.int64_max, i64::MAX);
    assert_eq!(parcelable.hexInt32_neg_1, -1);
    for i in parcelable.int8_1 {
        assert_eq!(i, 1);
    }
    for i in parcelable.int32_1 {
        assert_eq!(i, 1);
    }
    for i in parcelable.int64_1 {
        assert_eq!(i, 1);
    }
    assert_eq!(parcelable.hexInt32_pos_1, 1);
    assert_eq!(parcelable.hexInt64_pos_1, 1);
    assert_eq!(parcelable.const_exprs_1.0, 1);
    assert_eq!(parcelable.const_exprs_2.0, 1);
    assert_eq!(parcelable.const_exprs_3.0, 1);
    assert_eq!(parcelable.const_exprs_4.0, 1);
    assert_eq!(parcelable.const_exprs_5.0, 1);
    assert_eq!(parcelable.const_exprs_6.0, 1);
    assert_eq!(parcelable.const_exprs_7.0, 1);
    assert_eq!(parcelable.const_exprs_8.0, 1);
    assert_eq!(parcelable.const_exprs_9.0, 1);
    assert_eq!(parcelable.const_exprs_10.0, 1);
    assert_eq!(parcelable.addString1, "hello world!");
    assert_eq!(
        parcelable.addString2,
        "The quick brown fox jumps over the lazy dog."
    );

    assert_eq!(
        parcelable.shouldSetBit0AndBit2,
        StructuredParcelable::BIT0 | StructuredParcelable::BIT2
    );

    assert_eq!(parcelable.u, Some(Union::Union::Ns(vec![1, 2, 3])));
    assert_eq!(
        parcelable.shouldBeConstS1,
        Some(Union::Union::S(Union::S1.to_string()))
    )
}

#[test]
fn test_repeat_extendable_parcelable() {
    let service = get_test_service();

    let ext = Arc::new(MyExt {
        a: 42,
        b: "EXT".into(),
    });
    let mut ep = ExtendableParcelable {
        a: 1,
        b: "a".into(),
        c: 42,
        ..Default::default()
    };
    ep.ext
        .set_parcelable(Arc::clone(&ext))
        .expect("error setting parcelable");

    let mut ep2 = ExtendableParcelable::default();
    let result: std::prelude::v1::Result<(), Status> =
        service.RepeatExtendableParcelable(&ep, &mut ep2);
    assert_eq!(result, Ok(()));
    assert_eq!(ep2.a, ep.a);
    assert_eq!(ep2.b, ep.b);

    let ret_ext = ep2
        .ext
        .get_parcelable::<MyExt>()
        .expect("error getting parcelable");
    assert!(ret_ext.is_some());

    let ret_ext = ret_ext.unwrap();
    assert_eq!(ret_ext.a, ext.a);
    assert_eq!(ret_ext.b, ext.b);
}

macro_rules! test_parcelable_holder_stability {
    ($test:ident, $holder:path, $parcelable:path) => {
        #[test]
        fn $test() {
            let mut holder = <$holder>::default();
            let parcelable = Arc::new(<$parcelable>::default());
            let result = holder.ext.set_parcelable(Arc::clone(&parcelable));
            assert_eq!(result, Ok(()));

            let parcelable2 = holder.ext.get_parcelable::<$parcelable>().unwrap().unwrap();
            assert!(Arc::ptr_eq(&parcelable, &parcelable2));
        }
    };
}

test_parcelable_holder_stability! {
    test_vintf_parcelable_holder_can_contain_vintf_parcelable,
    VintfExtendableParcelable,
    VintfParcelable
}
test_parcelable_holder_stability! {
    test_stable_parcelable_holder_can_contain_vintf_parcelable,
    NonVintfExtendableParcelable,
    VintfParcelable
}
test_parcelable_holder_stability! {
    test_stable_parcelable_holder_can_contain_non_vintf_parcelable,
    NonVintfExtendableParcelable,
    NonVintfParcelable
}
test_parcelable_holder_stability! {
    test_stable_parcelable_holder_can_contain_unstable_parcelable,
    NonVintfExtendableParcelable,
    UnstableParcelable
}
test_parcelable_holder_stability! {
    test_unstable_parcelable_holder_can_contain_vintf_parcelable,
    UnstableExtendableParcelable,
    VintfParcelable
}
test_parcelable_holder_stability! {
    test_unstable_parcelable_holder_can_contain_non_vintf_parcelable,
    UnstableExtendableParcelable,
    NonVintfParcelable
}
test_parcelable_holder_stability! {
    test_unstable_parcelable_holder_can_contain_unstable_parcelable,
    UnstableExtendableParcelable,
    UnstableParcelable
}

#[test]
fn test_vintf_parcelable_holder_cannot_contain_not_vintf_parcelable() {
    let mut holder = VintfExtendableParcelable::default();
    let parcelable = Arc::new(NonVintfParcelable::default());
    let result = holder.ext.set_parcelable(Arc::clone(&parcelable));
    assert_eq!(result, Err(rsbinder::StatusCode::BadValue));

    let parcelable2 = holder.ext.get_parcelable::<NonVintfParcelable>();
    assert!(parcelable2.unwrap().is_none());
}

#[test]
fn test_vintf_parcelable_holder_cannot_contain_unstable_parcelable() {
    let mut holder = VintfExtendableParcelable::default();
    let parcelable = Arc::new(UnstableParcelable::default());
    let result = holder.ext.set_parcelable(Arc::clone(&parcelable));
    assert_eq!(result, Err(rsbinder::StatusCode::BadValue));

    let parcelable2 = holder.ext.get_parcelable::<UnstableParcelable>();
    assert!(parcelable2.unwrap().is_none());
}

#[test]
fn test_read_write_extension() {
    let ext = Arc::new(MyExt {
        a: 42,
        b: "EXT".into(),
    });
    let ext2 = Arc::new(MyExt2 {
        a: 42,
        b: MyExt {
            a: 24,
            b: "INEXT".into(),
        },
        c: "EXT2".into(),
    });

    let mut ep = ExtendableParcelable {
        a: 1,
        b: "a".into(),
        c: 42,
        ..Default::default()
    };

    ep.ext.set_parcelable(Arc::clone(&ext)).unwrap();
    ep.ext2.set_parcelable(Arc::clone(&ext2)).unwrap();

    let ext_like = ep.ext.get_parcelable::<MyExtLike>();
    assert_eq!(ext_like.unwrap_err(), rsbinder::StatusCode::BadValue);

    let actual_ext = ep.ext.get_parcelable::<MyExt>();
    assert!(actual_ext.unwrap().is_some());
    let actual_ext2 = ep.ext2.get_parcelable::<MyExt2>();
    assert!(actual_ext2.unwrap().is_some());

    check_extension_content(&ep, &ext, &ext2);

    let mut parcel = Parcel::new();
    ep.write_to_parcel(&mut parcel).unwrap();

    parcel.set_data_position(0);
    let mut ep1 = ExtendableParcelable::default();
    ep1.read_from_parcel(&mut parcel).unwrap();

    parcel.set_data_position(0);
    ep1.write_to_parcel(&mut parcel).unwrap();

    parcel.set_data_position(0);
    let mut ep2 = ExtendableParcelable::default();
    ep2.read_from_parcel(&mut parcel).unwrap();

    let ext_like = ep2.ext.get_parcelable::<MyExtLike>();
    assert!(ext_like.unwrap().is_none());

    let actual_ext = ep2.ext.get_parcelable::<MyExt>();
    assert!(actual_ext.unwrap().is_some());

    let new_ext2 = Arc::new(MyExt2 {
        a: 79,
        b: MyExt {
            a: 42,
            b: "INNEWEXT".into(),
        },
        c: "NEWEXT2".into(),
    });
    ep2.ext2.set_parcelable(Arc::clone(&new_ext2)).unwrap();

    check_extension_content(&ep1, &ext, &ext2);
    check_extension_content(&ep2, &ext, &new_ext2);
}

fn check_extension_content(ep: &ExtendableParcelable, ext: &MyExt, ext2: &MyExt2) {
    assert_eq!(ep.a, 1);
    assert_eq!(ep.b, "a");
    assert_eq!(ep.c, 42);

    let actual_ext = ep.ext.get_parcelable::<MyExt>().unwrap().unwrap();
    assert_eq!(ext.a, actual_ext.a);
    assert_eq!(ext.b, actual_ext.b);

    let actual_ext2 = ep.ext2.get_parcelable::<MyExt2>().unwrap().unwrap();
    assert_eq!(ext2.a, actual_ext2.a);
    assert_eq!(ext2.b.a, actual_ext2.b.a);
    assert_eq!(ext2.b.b, actual_ext2.b.b);
    assert_eq!(ext2.c, actual_ext2.c);
}

#[test]
fn test_reverse_recursive_list() {
    let service = get_test_service();

    let mut head = None;
    for n in 0..10 {
        let node = RecursiveList {
            value: n,
            next: head,
        };
        head = Some(Box::new(node));
    }
    // head = [9, 8, .., 0]
    let result = service.ReverseList(head.as_ref().unwrap());
    assert!(result.is_ok());

    // reversed should be [0, 1, ... 9]
    let mut reversed: Option<&RecursiveList> = result.as_ref().ok();
    for n in 0..10 {
        assert_eq!(reversed.map(|inner| inner.value), Some(n));
        reversed = reversed.unwrap().next.as_ref().map(|n| n.as_ref());
    }
    assert!(reversed.is_none())
}

#[test]
fn test_get_union_tags() {
    let service = get_test_service();
    let result = service.GetUnionTags(&[]);
    assert_eq!(result, Ok(vec![]));
    let result = service.GetUnionTags(&[Union::Union::N(0), Union::Union::Ns(vec![])]);
    assert_eq!(result, Ok(vec![Union::Tag::n, Union::Tag::ns]));
}

#[test]
fn test_unions() {
    assert_eq!(Union::Union::default(), Union::Union::Ns(vec![]));
    assert_eq!(EnumUnion::default(), EnumUnion::IntEnum(IntEnum::FOO));
}

const EXPECTED_ARG_VALUE: i32 = 100;
const EXPECTED_RETURN_VALUE: i32 = 200;

struct TestDefaultImpl;

impl rsbinder::Interface for TestDefaultImpl {}

impl ITestServiceDefault for TestDefaultImpl {
    fn UnimplementedMethod(&self, arg: i32) -> std::result::Result<i32, Status> {
        assert_eq!(arg, EXPECTED_ARG_VALUE);
        Ok(EXPECTED_RETURN_VALUE)
    }
}

#[test]
fn test_default_impl() {
    let service = get_test_service();
    let di: ITestServiceDefaultRef = Arc::new(TestDefaultImpl);
    <BpTestService as ITestService::ITestService>::setDefaultImpl(di);

    let result = service.UnimplementedMethod(EXPECTED_ARG_VALUE);
    assert_eq!(result, Ok(EXPECTED_RETURN_VALUE));
}

// Not supported version and hash yet.
/*
#[test]
fn test_versioned_interface_version() {
    let service: rsbinder::Strong<dyn IFooInterface::IFooInterface> =
        hub::get_interface(<BpFooInterface as IFooInterface::IFooInterface>::descriptor())
            .expect("did not get binder service");

    let version = service.getInterfaceVersion();
    assert_eq!(version, Ok(1));
}

#[test]
fn test_versioned_interface_hash() {
    let service: rsbinder::Strong<dyn IFooInterface::IFooInterface> =
        hub::get_interface(<BpFooInterface as IFooInterface::IFooInterface>::descriptor())
            .expect("did not get binder service");

    let hash = service.getInterfaceHash();
    assert_eq!(hash.as_ref().map(String::as_str), Ok("9e7be1859820c59d9d55dd133e71a3687b5d2e5b"));
}
*/

#[test]
fn test_versioned_known_union_field_is_ok() {
    init_test();
    let service: rsbinder::Strong<dyn IFooInterface::IFooInterface> =
        hub::get_interface(<BpFooInterface as IFooInterface::IFooInterface>::descriptor())
            .expect("did not get binder service");

    assert_eq!(
        service.acceptUnionAndReturnString(&BazUnion::IntNum(42)),
        Ok(String::from("42"))
    );
}

// #[test]
// fn test_versioned_unknown_union_field_triggers_error() {
//     init_test();
//     let service: rsbinder::Strong<dyn IFooInterface::IFooInterface> =
//         hub::get_interface(<BpFooInterface as IFooInterface::IFooInterface>::descriptor())
//             .expect("did not get binder service");

//     let ret = service.acceptUnionAndReturnString(&BazUnion::LongNum(42));
//     assert!(ret.is_err());

//     let main_service = get_test_service();
//     let backend = main_service.getBackendType().expect("error getting backend type");

//     // b/173458620 - for investigation of fixing difference
//     if backend == BackendType::JAVA {
//         assert_eq!(ret.unwrap_err().exception_code(), rsbinder::ExceptionCode::IllegalArgument);
//     } else {
//         assert_eq!(ret.unwrap_err().transaction_error(), rsbinder::StatusCode::BadValue);
//     }
// }

#[test]
fn test_array_of_parcelable_with_new_field() {
    init_test();
    let service: rsbinder::Strong<dyn IFooInterface::IFooInterface> =
        hub::get_interface(<BpFooInterface as IFooInterface::IFooInterface>::descriptor())
            .expect("did not get binder service");

    let foos = [Default::default(), Default::default(), Default::default()];
    let ret = service.returnsLengthOfFooArray(&foos);
    assert_eq!(ret, Ok(foos.len() as i32));
}

#[test]
fn test_read_data_correctly_after_parcelable_with_new_field() {
    init_test();
    let service: rsbinder::Strong<dyn IFooInterface::IFooInterface> =
        hub::get_interface(<BpFooInterface as IFooInterface::IFooInterface>::descriptor())
            .expect("did not get binder service");

    let in_foo = Default::default();
    let mut inout_foo = Foo { intDefault42: 0 };
    let mut out_foo = Foo { intDefault42: 0 };
    let ret = service.ignoreParcelablesAndRepeatInt(&in_foo, &mut inout_foo, &mut out_foo, 43);
    assert_eq!(ret, Ok(43));
    assert_eq!(inout_foo.intDefault42, 0);
    // Android testcase is 0, but I think it must be 42.
    // Out parameter is not sent the value 0 to the service, and the service initializes it with default value 42.
    // And, the default value of the service is sent to the client.
    // If I am wrong, please change it.
    assert_eq!(out_foo.intDefault42, 42);
}

fn test_renamed_interface<F>(f: F)
where
    F: FnOnce(rsbinder::Strong<dyn IOldName::IOldName>, rsbinder::Strong<dyn INewName::INewName>),
{
    let service = get_test_service();
    let old_name = service.GetOldNameInterface();
    assert!(old_name.is_ok());

    let new_name = service.GetNewNameInterface();
    assert!(new_name.is_ok());

    f(old_name.unwrap(), new_name.unwrap());
}

#[test]
fn test_renamed_interface_old_as_old() {
    test_renamed_interface(|old_name, _| {
        assert_eq!(
            <BpOldName as IOldName::IOldName>::descriptor(),
            "android.aidl.tests.IOldName"
        );

        let real_name = old_name.RealName();
        assert_eq!(real_name.as_ref().map(String::as_str), Ok("OldName"));
    });
}

#[test]
fn test_renamed_interface_new_as_new() {
    test_renamed_interface(|_, new_name| {
        assert_eq!(
            <BpNewName as INewName::INewName>::descriptor(),
            "android.aidl.tests.IOldName"
        );

        let real_name = new_name.RealName();
        assert_eq!(real_name.as_ref().map(String::as_str), Ok("NewName"));
    });
}

#[test]
fn test_renamed_interface_old_as_new() {
    test_renamed_interface(|old_name, _| {
        let new_name = old_name
            .as_binder()
            .into_interface::<dyn INewName::INewName>();
        assert!(new_name.is_ok());

        let real_name = new_name.unwrap().RealName();
        assert_eq!(real_name.as_ref().map(String::as_str), Ok("OldName"));
    });
}

#[test]
fn test_renamed_interface_new_as_old() {
    test_renamed_interface(|_, new_name| {
        let old_name = new_name
            .as_binder()
            .into_interface::<dyn IOldName::IOldName>();
        assert!(old_name.is_ok());

        let real_name = old_name.unwrap().RealName();
        assert_eq!(real_name.as_ref().map(String::as_str), Ok("NewName"));
    });
}

#[derive(Debug, Default)]
struct Callback {
    received: Arc<Mutex<Option<ParcelableWithNested::Status::Status>>>,
}

impl Interface for Callback {}

impl INestedService::ICallback::ICallback for Callback {
    fn done(&self, st: ParcelableWithNested::Status::Status) -> std::result::Result<(), Status> {
        *self.received.lock().unwrap() = Some(st);
        Ok(())
    }
}

#[test]
fn test_nested_type() {
    let service: rsbinder::Strong<dyn INestedService::INestedService> = hub::get_interface(
        <INestedService::BpNestedService as INestedService::INestedService>::descriptor(),
    )
    .expect("did not get binder service");

    let p = ParcelableWithNested::ParcelableWithNested {
        status: ParcelableWithNested::Status::Status::OK,
    };
    // OK -> NOT_OK
    let ret = service.flipStatus(&p);
    assert_eq!(
        ret,
        Ok(INestedService::Result::Result {
            status: ParcelableWithNested::Status::Status::NOT_OK
        })
    );
    let received = Arc::new(Mutex::new(None));
    // NOT_OK -> OK with nested callback interface
    let cb = INestedService::ICallback::BnCallback::new_binder(Callback {
        received: Arc::clone(&received),
    });
    let ret = service.flipStatusWithCallback(ParcelableWithNested::Status::Status::NOT_OK, &cb);
    assert_eq!(ret, Ok(()));
    let received = received.lock().unwrap();
    assert_eq!(*received, Some(ParcelableWithNested::Status::Status::OK))
}

#[test]
fn test_nonnull_binder() {
    let service = get_test_service();
    let result = service.TakesAnIBinder(&service.as_binder());
    assert!(result.is_ok());
}

#[test]
fn test_binder_list_without_null() {
    let service = get_test_service();
    let result = service.TakesAnIBinderList(&[service.as_binder()]);
    assert!(result.is_ok());
}

#[test]
fn test_null_binder_to_annotated_method() {
    let service = get_test_service();
    let result = service.TakesANullableIBinder(None);
    assert!(result.is_ok());
}

#[test]
fn test_binder_list_with_null_to_annotated_method() {
    let service = get_test_service();
    let result = service.TakesANullableIBinderList(Some(&[Some(service.as_binder()), None]));
    assert!(result.is_ok());
}

#[test]
fn test_binder_array() {
    let service = get_test_service();
    let callback = service
        .GetCallback(false)
        .expect("error calling GetCallback")
        .expect("expected Some from GetCallback");

    let mut array = vec![service.as_binder(), callback.as_binder()];

    // Java needs initial values here (can't resize arrays)
    let mut repeated = vec![Default::default(); array.len()];

    let result = service.ReverseIBinderArray(&array, &mut repeated);
    assert_eq!(
        repeated.into_iter().collect::<Option<Vec<_>>>().as_ref(),
        Some(&array)
    );
    array.reverse();
    assert_eq!(result, Ok(array));
}

#[test]
fn test_nullable_binder_array() {
    let service = get_test_service();
    let mut array = vec![Some(service.as_binder()), None];

    // Java needs initial values here (can't resize arrays)
    let mut repeated = Some(vec![Default::default(); array.len()]);

    let result = service.ReverseNullableIBinderArray(Some(&array[..]), &mut repeated);
    assert_eq!(repeated.as_ref(), Some(&array));
    array.reverse();
    assert_eq!(result, Ok(Some(array)));
}

#[test]
fn test_read_write_fixed_size_array() {
    init_logger();
    let mut parcel = Parcel::new();
    let mut p: FixedSizeArrayExample = Default::default();
    p.byteMatrix[0][0] = 0;
    p.byteMatrix[0][1] = 1;
    p.byteMatrix[1][0] = 2;
    p.byteMatrix[1][1] = 3;

    p.floatMatrix[0][0] = 0.0;
    p.floatMatrix[0][1] = 1.0;
    p.floatMatrix[1][0] = 2.0;
    p.floatMatrix[1][1] = 3.0;

    p.boolNullableArray = Some([true, false]);
    p.byteNullableArray = Some([42, 0]);
    p.stringNullableArray = Some([Some("hello".into()), Some("world".into())]);

    p.boolNullableMatrix = Some([[true, false], Default::default()]);
    p.byteNullableMatrix = Some([[42, 0], Default::default()]);
    p.stringNullableMatrix = Some([
        [Some("hello".into()), Some("world".into())],
        Default::default(),
    ]);

    assert_eq!(parcel.write(&p), Ok(()));
    parcel.set_data_position(0);
    assert_eq!(p, parcel.read::<FixedSizeArrayExample>().unwrap());
}

#[test]
fn test_fixed_size_array_uses_array_optimization() {
    let mut parcel = Parcel::new();
    let byte_array = [[1u8, 2u8, 3u8], [4u8, 5u8, 6u8]];
    assert_eq!(parcel.write(&byte_array), Ok(()));
    parcel.set_data_position(0);
    assert_eq!(parcel.read::<i32>(), Ok(2i32));
    assert_eq!(parcel.read::<Vec<u8>>(), Ok(vec![1u8, 2u8, 3u8]));
    assert_eq!(parcel.read::<Vec<u8>>(), Ok(vec![4u8, 5u8, 6u8]));
}

macro_rules! test_repeat_fixed_size_array {
    ($service:ident, $func:ident, $value:expr) => {
        let array = $value;
        let mut repeated = Default::default();
        let result = $service.$func(&array, &mut repeated).unwrap();
        assert_eq!(repeated, array);
        assert_eq!(result, array);
    };
}

macro_rules! test_repeat_fixed_size_array_1d_binder {
    ($service:ident, $func:ident, $value:expr) => {
        let array = $value;
        let mut repeated = Default::default();
        let result = $service.$func(&array, &mut repeated).unwrap();
        assert_eq!(result, array.clone());
        assert_eq!(repeated, array.map(Some));
    };
}

macro_rules! test_repeat_fixed_size_array_2d_binder {
    ($service:ident, $func:ident, $value:expr) => {
        let array = $value;
        let mut repeated = Default::default();
        let result = $service.$func(&array, &mut repeated).unwrap();
        assert_eq!(result, array.clone());
        assert_eq!(repeated, array.map(|row| row.map(Some)));
    };
}

#[test]
fn test_fixed_size_array_over_binder() {
    let test_service = get_test_service();
    let service: rsbinder::Strong<dyn IRepeatFixedSizeArray> =
        hub::get_interface(<BpRepeatFixedSizeArray as IRepeatFixedSizeArray>::descriptor())
            .expect("did not get binder service");

    test_repeat_fixed_size_array!(service, RepeatBytes, [1u8, 2u8, 3u8]);
    test_repeat_fixed_size_array!(service, RepeatInts, [1i32, 2i32, 3i32]);

    let binder1 = test_service.as_binder();
    let binder2 = test_service
        .GetCallback(false)
        .expect("error calling GetCallback")
        .expect("expected Some from GetCallback")
        .as_binder();
    let binder3 = service.as_binder();
    test_repeat_fixed_size_array_1d_binder!(
        service,
        RepeatBinders,
        [binder1.clone(), binder2.clone(), binder3.clone()]
    );

    let p1 = IntParcelable { value: 1 };
    let p2 = IntParcelable { value: 2 };
    let p3 = IntParcelable { value: 3 };
    test_repeat_fixed_size_array!(service, RepeatParcelables, [p1, p2, p3]);

    test_repeat_fixed_size_array!(service, Repeat2dBytes, [[1u8, 2u8, 3u8], [1u8, 2u8, 3u8]]);
    test_repeat_fixed_size_array!(
        service,
        Repeat2dInts,
        [[1i32, 2i32, 3i32], [1i32, 2i32, 3i32]]
    );

    test_repeat_fixed_size_array_2d_binder!(
        service,
        Repeat2dBinders,
        [
            [binder1.clone(), binder2.clone(), binder3.clone()],
            [binder1, binder2, binder3]
        ]
    );

    test_repeat_fixed_size_array!(service, Repeat2dParcelables, [[p1, p2, p3], [p1, p2, p3]]);
}

#[test]
fn test_ping() {
    let test_service = get_test_service();
    assert_eq!(test_service.as_binder().ping_binder(), Ok(()));
}

#[test]
fn test_dump() {
    let test_service = get_test_service();
    let (mut read_file, write_file) = build_pipe();

    let args = vec!["dump".to_owned(), "ITestService".to_owned()];
    let expected = args.join("\n") + "\n";

    test_service
        .as_binder()
        .as_proxy()
        .unwrap()
        .dump(write_file, &args)
        .unwrap();
    let mut buf = String::new();
    read_file.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, expected);
}

#[test]
#[ignore]
fn test_death_recipient() {
    let test_service = get_test_service();
    let (mut read_file, write_file) = build_pipe();

    struct MyDeathRecipient {
        write_file: Mutex<File>,
    }

    impl DeathRecipient for MyDeathRecipient {
        fn binder_died(&self, _who: &WIBinder) {
            let mut writer = self.write_file.lock().unwrap();

            writer.write_all(b"binder_died\n").unwrap();
            // writer.flush().unwrap();
        }
    }

    {
        let recipient = Arc::new(MyDeathRecipient {
            write_file: Mutex::new(write_file),
        });
        test_service
            .as_binder()
            .link_to_death(Arc::downgrade(
                &(recipient.clone() as Arc<dyn DeathRecipient>),
            ))
            .unwrap();
        test_service
            .as_binder()
            .unlink_to_death(Arc::downgrade(
                &(recipient.clone() as Arc<dyn DeathRecipient>),
            ))
            .unwrap();

        test_service
            .as_binder()
            .link_to_death(Arc::downgrade(
                &(recipient.clone() as Arc<dyn DeathRecipient>),
            ))
            .unwrap();

        println!("Killing the service...");
        test_service.killService().unwrap();

        println!("Waiting for the service to die...");
        std::thread::sleep(std::time::Duration::from_secs(1));
        let result = test_service.as_binder().unlink_to_death(Arc::downgrade(
            &(recipient.clone() as Arc<dyn DeathRecipient>),
        ));
        assert_eq!(result, Err(rsbinder::StatusCode::DeadObject));
    }

    let mut buf = String::new();
    read_file.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, "binder_died\n");
}

#[test]
fn test_hub() {
    hub::get_service(ITestService::BpTestService::descriptor()).unwrap();
    let list = hub::list_services(hub::DUMP_FLAG_PRIORITY_DEFAULT);
    assert!(list
        .iter()
        .any(|s| s == ITestService::BpTestService::descriptor()));

    #[cfg(target_os = "android")]
    if get_android_sdk_version() < 31 {
        // On Android 11 and below, the service debug info is not available.
        return;
    }

    let service_debug_info_list = hub::get_service_debug_info().unwrap();
    assert!(service_debug_info_list
        .iter()
        .any(|s| s.name == ITestService::BpTestService::descriptor()));
}

/// Test for issue #47: cached interface string bug
///
/// Originally reproduced the case where `handle_to_proxy` returned a
/// stale proxy with an incorrect descriptor when handles were reused
/// for different services. The bug pattern below intentionally drove
/// the kernel weak count for each service handle to zero between
/// resolutions:
///
/// ```ignore
/// let service_manager = rsbinder::hub::default();
/// let all_services = service_manager.list_services(0xf);
/// for service_name in &all_services {
///     let service = service_manager.get_service(service_name).unwrap();
///     println!("{} interface: {}", service_name, service.descriptor());
///     service.dec_weak().unwrap();
/// }
/// ```
///
/// Under the cache-pin model introduced by this PR, `dec_weak()` is a
/// no-op on proxies — kernel weak refs are owned by the process-wide
/// cache pin, not by user-side `WIBinder` clones. The test now
/// vacuously demonstrates that lookup-after-zero is structurally safe:
/// the cache `Weak<ProxyHandle>` may dangle, but the cache pin keeps
/// `binder_ref(handle).weak >= 1`, so resurrection (slow-path case (b))
/// reuses the cached descriptor and issues a fresh `BC_ACQUIRE` against
/// a still-alive kernel slot. The `dec_weak()` calls below are kept as
/// non-functional historical markers — if a future change accidentally
/// re-introduces a code path where `dec_weak` is non-trivial on
/// proxies, this test will exercise it.
#[test]
fn test_issue_47_cached_interface_string() {
    init_test();

    // Get list of all available services
    let all_services = hub::list_services(hub::DUMP_FLAG_PRIORITY_DEFAULT);

    if all_services.len() < 2 {
        println!(
            "Skipping test: need at least 2 services, found {}",
            all_services.len()
        );
        return;
    }

    println!("\nTesting issue #47 with {} services", all_services.len());

    // Track services and their expected descriptors
    let mut service_descriptors = Vec::new();

    // First pass: get services and record their descriptors
    println!("\n=== First Pass: Recording service descriptors ===");
    for service_name in all_services.iter().take(10) {
        if let Some(service) = hub::get_service(service_name) {
            let descriptor = service.descriptor().to_string();

            println!("Service '{}' -> descriptor: '{}'", service_name, descriptor);

            service_descriptors.push((service_name.clone(), descriptor));

            // Release the service (this is important to potentially trigger handle reuse)
            let _ = service.dec_weak();
        }
    }

    // Second pass: retrieve services again and verify descriptors match
    println!("\n=== Second Pass: Verifying descriptors ===");
    let mut bug_detected = false;

    for (service_name, expected_descriptor) in &service_descriptors {
        if let Some(service) = hub::get_service(service_name) {
            let actual_descriptor = service.descriptor();

            println!(
                "Service '{}' -> descriptor: '{}' (expected: '{}')",
                service_name, actual_descriptor, expected_descriptor
            );

            // Check if descriptors match
            if actual_descriptor != expected_descriptor {
                println!("\n!!! BUG #47 DETECTED !!!");
                println!("Service '{}' has wrong descriptor!", service_name);
                println!("  Expected: '{}'", expected_descriptor);
                println!("  Got:      '{}'", actual_descriptor);
                bug_detected = true;
            }

            let _ = service.dec_weak();
        }
    }

    // This assertion will FAIL if bug #47 exists
    assert!(
        !bug_detected,
        "BUG #47 DETECTED: Some services returned incorrect descriptors. \
         This indicates the proxy cache is returning stale proxies with wrong interface strings."
    );

    println!("\n=== Test passed: All services have correct descriptors ===\n");
}

// Binder extension tests (Issue #8)

// Local helper for binder extension tests
struct ExtNamedCallback(String);

impl rsbinder::Interface for ExtNamedCallback {}

impl INamedCallback::INamedCallback for ExtNamedCallback {
    fn GetName(&self) -> std::result::Result<String, Status> {
        Ok(self.0.clone())
    }
}

// Test 1: Local binder set/get extension
#[test]
fn test_binder_extension_local_set_get() {
    let service = INamedCallback::BnNamedCallback::new_binder(ExtNamedCallback("main".into()));
    let ext = INamedCallback::BnNamedCallback::new_binder(ExtNamedCallback("ext".into()));

    let binder = service.as_binder();
    assert!(binder.set_extension(&ext.as_binder()).is_ok());

    let got = binder.get_extension();
    assert!(got.is_ok());
    let got = got.unwrap();
    assert!(got.is_some(), "Extension should be set");
    assert_eq!(
        got.unwrap().descriptor(),
        "android.aidl.tests.INamedCallback"
    );
}

// Test 2: Local binder without extension returns None
#[test]
fn test_binder_extension_local_default_none() {
    let service = INamedCallback::BnNamedCallback::new_binder(ExtNamedCallback("main".into()));
    let binder = service.as_binder();

    let ext = binder.get_extension();
    assert!(ext.is_ok());
    assert!(
        ext.unwrap().is_none(),
        "Extension should be None by default"
    );
}

// Test 3: Remote get_extension with extension set (EXTENSION_TRANSACTION round-trip)
#[test]
fn test_binder_extension_get_from_remote() {
    let service = get_test_service();
    let binder = service.as_binder();

    // The test_service sets a NamedCallback("binder_ext") as extension
    let ext = binder.get_extension();
    assert!(ext.is_ok());
    let ext = ext.unwrap();
    assert!(ext.is_some(), "Extension should be set on test_service");

    // Verify the extension is a valid remote binder
    let ext_binder = ext.unwrap();
    assert!(
        ext_binder.is_remote(),
        "Extension should be a remote binder"
    );
    assert_eq!(ext_binder.descriptor(), "android.aidl.tests.INamedCallback");
}

// Test 4: Remote get_extension without extension (null binder round-trip)
#[test]
fn test_binder_extension_none_from_remote() {
    init_test();
    // The versioned service does NOT set any extension
    let service: rsbinder::Strong<dyn IFooInterface::IFooInterface> =
        hub::get_interface(<BpFooInterface as IFooInterface::IFooInterface>::descriptor())
            .expect("did not get binder service");

    let ext = service.as_binder().get_extension();
    assert!(ext.is_ok());
    assert!(
        ext.unwrap().is_none(),
        "Extension should be None for service without extension"
    );
}

// Test 5: Proxy cache hit (extension exists)
#[test]
fn test_binder_extension_proxy_cache_with_ext() {
    let service = get_test_service();
    let binder = service.as_binder();

    // First call: triggers EXTENSION_TRANSACTION
    let ext1 = binder.get_extension().unwrap();
    assert!(ext1.is_some());

    // Second call: should return cached result (no remote query)
    let ext2 = binder.get_extension().unwrap();
    assert!(ext2.is_some());

    // Both should return the same extension descriptor
    assert_eq!(ext1.unwrap().descriptor(), ext2.unwrap().descriptor());
}

// Test 6: Proxy cache hit (no extension)
#[test]
fn test_binder_extension_proxy_cache_without_ext() {
    init_test();
    let service: rsbinder::Strong<dyn IFooInterface::IFooInterface> =
        hub::get_interface(<BpFooInterface as IFooInterface::IFooInterface>::descriptor())
            .expect("did not get binder service");
    let binder = service.as_binder();

    // First call: triggers EXTENSION_TRANSACTION, caches None
    let ext1 = binder.get_extension().unwrap();
    assert!(ext1.is_none());

    // Second call: should return cached None (no remote query)
    let ext2 = binder.get_extension().unwrap();
    assert!(ext2.is_none());
}

// Test 8: Extension binder can be used as a typed interface
#[test]
fn test_binder_extension_use_as_interface() {
    let service = get_test_service();
    let ext = service.as_binder().get_extension().unwrap().unwrap();

    // Cast extension to INamedCallback and call GetName()
    let named_callback: rsbinder::Strong<dyn INamedCallback::INamedCallback> = ext
        .into_interface()
        .expect("Extension should be castable to INamedCallback");
    let name = named_callback.GetName().expect("GetName should succeed");
    assert_eq!(name, "binder_ext");
}

// =============================================================================
// Cache-pin model integration tests (FOLLOW_UP_PR_100 test plan #1, #2, #3, #5)
// =============================================================================
//
// These tests validate the cache-pin model on real binderfs. They require:
// - A live `test_service` registered with `rsb_hub`
// - The kernel's `BINDER_GET_NODE_INFO_FOR_REF` ioctl
//
// They are run as part of the standard `cargo test --package tests` invocation
// in `.github/workflows/integration-test.yml`.

/// Test plan #2 — kernel ref-count consistency under the cache-pin model.
///
/// Verifies that user-space `SIBinder` clones do NOT raise the kernel
/// strong count: under the cache-pin model the kernel observes exactly
/// **one `BC_ACQUIRE` per `Arc<ProxyHandle>` lifetime**, regardless of
/// how many `SIBinder` / `Strong<I>` clones exist. Pre-PR (PR #100 and
/// earlier), each clone drove its own `RefCounter.strong` cycle which
/// could elevate the kernel count.
///
/// Sampling discipline: every command must reach the kernel before we
/// read the count, so we issue a `ping_binder()` (which forces a driver
/// round-trip) before each `strong_ref_count_for_node` query. This is
/// the test-side analog of plan §4's `barrier_then_sample`.
#[test]
fn test_kernel_strong_ref_count_one_per_proxy_handle() {
    init_test();

    let service = get_test_service();
    let binder = service.as_binder();
    binder.ping_binder().expect("ping must succeed");

    // SIBinder Derefs to dyn IBinder, which has as_proxy() returning
    // Option<&ProxyHandle>. The borrow is valid for as long as
    // `binder` (which holds the underlying Arc) is alive.
    let proxy_ref: &rsbinder::proxy::ProxyHandle = binder
        .as_proxy()
        .expect("test_service binder must be a proxy");

    // BINDER_GET_NODE_INFO_FOR_REF requires CAP_SYS_NICE (or root) on
    // mainline Linux — GitHub Actions Ubuntu runners run as the
    // unprivileged `runner` user and the ioctl returns EPERM. Skip
    // the test in that case rather than failing CI; the cache-pin
    // model's invariants are independently exercised by the race
    // reproducer + case-(b) round-trip tests above (no privileged
    // ioctl required) and by the loom PoC. Real-device coverage
    // (Android emulators, root Linux setups) still validates the
    // kernel-side counts via this test.
    let count_initial = match ProcessState::as_self().strong_ref_count_for_node(proxy_ref) {
        Ok(n) => n,
        // EPERM maps to StatusCode::PermissionDenied (see
        // rsbinder/src/error.rs's `From<rustix::io::Errno>` impl).
        // Errno(EPERM) is therefore unreachable here.
        Err(rsbinder::StatusCode::PermissionDenied) => {
            eprintln!(
                "skipping test_kernel_strong_ref_count_one_per_proxy_handle: \
                 BINDER_GET_NODE_INFO_FOR_REF returned EPERM (need root / CAP_SYS_NICE). \
                 Cache-pin invariants are still covered by race-reproducer + case-(b) tests."
            );
            return;
        }
        Err(other) => panic!("ioctl failed unexpectedly: {other:?}"),
    };
    assert!(
        count_initial >= 1,
        "kernel must report strong >= 1 while at least one Arc<ProxyHandle> is alive; \
         got {count_initial}"
    );

    // Clone a few SIBinders. Under the cache-pin model these are pure
    // Arc::clone — no kernel command. Kernel strong count must NOT
    // change.
    let _clone1 = binder.clone();
    let _clone2 = binder.clone();
    let _clone3 = binder.clone();
    binder
        .ping_binder()
        .expect("ping after clones must succeed");

    let count_after_clones = ProcessState::as_self()
        .strong_ref_count_for_node(proxy_ref)
        .expect("ioctl must succeed (we already verified permission above)");
    assert_eq!(
        count_after_clones, count_initial,
        "kernel strong count must NOT rise on SIBinder clone (cache-pin model invariant); \
         initial={count_initial} after_clones={count_after_clones}"
    );

    drop(_clone1);
    drop(_clone2);
    drop(_clone3);
    binder.ping_binder().expect("ping after drops must succeed");

    let count_after_drops = ProcessState::as_self()
        .strong_ref_count_for_node(proxy_ref)
        .expect("ioctl must succeed");
    assert_eq!(
        count_after_drops, count_initial,
        "kernel strong count must NOT fall on SIBinder drop while parent Arc is alive; \
         initial={count_initial} after_drops={count_after_drops}"
    );
}

/// Test plan #1 — race reproducer for slow-path case (b).
///
/// Under master pre-PR-#100 with aggressive concurrency, the cache
/// could hand out a fresh `ProxyHandle` whose handle had been freed
/// by a racing `BC_RELEASE`, surfacing as `DeadObject` / `BadType` /
/// wrong descriptor. Under the cache-pin model the cache pin keeps
/// the kernel slot alive across `strong = 0` windows, so every
/// resurrection (case b) succeeds and yields the original descriptor.
///
/// Stress strategy:
///
/// - **High thread count** (N=16): two threads per typical CI vCPU
///   on a 2-vCPU runner, ample preemption pressure.
/// - **Real transactions** (every iteration): each iteration runs an
///   actual `RepeatString` transaction. If a race surfaced as a
///   freed-but-cached handle, the kernel would reject the transaction
///   with `DeadObject`, surfacing as a panic on the `expect("RepeatString
///   must succeed")` instead of a silent descriptor-only check.
/// - **Arc-identity invariant** (every iteration): two back-to-back
///   lookups must yield the same `ProxyHandle` allocation while at
///   least one Strong is alive. This catches a regression where the
///   resurrection path accidentally allocates a fresh `ProxyHandle`
///   while another thread still holds one.
/// - **Immediate drop** drives the cache `Weak` to dangling between
///   iterations, so subsequent lookups exercise case (b).
#[test]
fn test_cache_pin_race_reproducer_no_descriptor_mismatch() {
    init_test();

    // Pre-resolve once to ensure the service is registered and a cache
    // entry exists; subsequent threads will mostly hit the read fast
    // path or case (b).
    let _seed = get_test_service();

    const N: usize = 16;
    const K: usize = 100;
    let expected = <BpTestService as ITestService::ITestService>::descriptor().to_string();
    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let expected = expected.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..K {
                let svc: rsbinder::Strong<dyn ITestService::ITestService> =
                    hub::get_interface(<BpTestService as ITestService::ITestService>::descriptor())
                        .expect("hub::get_interface must succeed");
                let binder1 = svc.as_binder();
                let actual = binder1.descriptor().to_string();
                assert_eq!(
                    actual, expected,
                    "iteration {i}: descriptor mismatch; got '{actual}' expected '{expected}'"
                );

                // Arc-identity invariant: a second lookup MUST return
                // the same ProxyHandle while `svc` (and hence its
                // Arc<ProxyHandle>) is alive. Production cache stores
                // sync::Weak<ProxyHandle> exactly so that two
                // concurrent lookups yielding live Arcs share the
                // same allocation.
                let svc2: rsbinder::Strong<dyn ITestService::ITestService> =
                    hub::get_interface(<BpTestService as ITestService::ITestService>::descriptor())
                        .expect("hub::get_interface must succeed (second lookup)");
                assert_eq!(
                    svc.as_binder(),
                    svc2.as_binder(),
                    "iteration {i}: Arc-identity invariant broken — two concurrent \
                     lookups yielded different ProxyHandle allocations while both \
                     Strong<ITestService> were alive"
                );
                drop(svc2);

                // Real transaction — if the handle were freed-but-cached
                // (the race this PR closes), the kernel would reject
                // and `expect` would panic.
                let echoed = svc
                    .RepeatString("rsbinder-cache-pin")
                    .expect("RepeatString must succeed across resurrections");
                assert_eq!(echoed, "rsbinder-cache-pin");

                // Immediate drop drives the cache `Weak` to dangling
                // before the next iteration in this thread (and
                // potentially before another thread's lookup).
            }
        }));
    }
    for h in handles {
        h.join().expect("worker thread must not panic");
    }
}

/// Test plan #5 — barrier-coordinated case (b) variant.
///
/// Tighter than the bulk reproducer: explicitly drives the cache
/// `Weak` to dangling between rounds, then re-resolves to force
/// case (b). Each round resurrects from the cached descriptor (no
/// new INTERFACE_TRANSACTION, no new BC_INCREFS). Verifies the
/// resurrection succeeds and yields a binder with the original
/// descriptor.
#[test]
fn test_cache_pin_case_b_resurrection_round_trip() {
    init_test();
    let canonical = <BpTestService as ITestService::ITestService>::descriptor().to_string();

    for round in 0..50 {
        let svc: rsbinder::Strong<dyn ITestService::ITestService> =
            hub::get_interface(<BpTestService as ITestService::ITestService>::descriptor())
                .expect("hub::get_interface must succeed");
        let descriptor = svc.as_binder().descriptor().to_string();
        assert_eq!(
            descriptor, canonical,
            "round {round}: case-(a)-or-(b) lookup yielded wrong descriptor"
        );
        svc.as_binder()
            .ping_binder()
            .expect("ping after resurrection must succeed");
        // Drop forces the cache `Weak` to dangling; next round hits
        // case (b) (entry present, dead Arc).
        drop(svc);
    }
}

/// `WIBinder::upgrade()` for proxies must succeed across user-side
/// `drop(strong)` as long as the cache entry (and therefore the
/// kernel `binder_ref` slot's `BC_INCREFS` pin) is alive — matching
/// Android `wp<BpBinder>::promote()` semantics.
///
/// Pre-PR `WIBinder::upgrade()` succeeded *unconditionally* because
/// `WIBinder` held a strong Arc in disguise. The cache-pin refactor
/// initially made it `sync::Weak::upgrade` only, which returned
/// `DeadObject` once every user-side `Strong` was dropped — too
/// strict, because the cache pin had already structurally guaranteed
/// the kernel slot stayed alive. This test verifies the corrected
/// semantics: `upgrade()` routes through the proxy cache and yields
/// a freshly resurrected `Arc<ProxyHandle>` (different allocation,
/// same kernel binder_node).
#[test]
fn test_weak_upgrade_resurrects_proxy_after_strong_drop() {
    init_test();

    let canonical = <BpTestService as ITestService::ITestService>::descriptor().to_string();
    let svc = get_test_service();
    let weak: WIBinder = SIBinder::downgrade(&svc.as_binder());
    drop(svc);

    // After `drop(svc)` the only `Arc<ProxyHandle>` is gone, but the
    // cache entry is still there with its BC_INCREFS pin keeping
    // `binder_ref(handle).weak >= 1`. `upgrade()` must succeed via
    // case-(b) resurrection.
    let resurrected = weak
        .upgrade()
        .expect("weak.upgrade must succeed while cache entry alive");

    // Descriptor preserved across resurrection (cached, no new
    // INTERFACE_TRANSACTION).
    assert_eq!(
        resurrected.descriptor(),
        canonical,
        "resurrected proxy must carry the original descriptor"
    );

    // Transaction succeeds, proving the resurrected proxy refers to
    // the same kernel binder_node.
    resurrected
        .ping_binder()
        .expect("ping after resurrection must succeed");
}

/// Two `WIBinder` clones for the same underlying handle compare
/// equal under `PartialEq`, even after the original strong is
/// dropped and resurrection has produced a different `Arc<ProxyHandle>`.
/// Identity is `(handle, generation)`, matching Android `BpBinder`
/// identity (stable across resurrection).
#[test]
fn test_weak_partial_eq_handle_identity_across_resurrection() {
    init_test();

    let svc1 = get_test_service();
    let weak1: WIBinder = SIBinder::downgrade(&svc1.as_binder());
    drop(svc1);

    // Resurrect and downgrade again — fresh Arc<ProxyHandle>, but the
    // cache entry's generation is preserved (same kernel slot), so the
    // new WIBinder snapshots the same generation.
    let svc2 = get_test_service();
    let weak2: WIBinder = SIBinder::downgrade(&svc2.as_binder());

    assert_eq!(
        weak1, weak2,
        "WIBinder for the same kernel binder_node must be equal across resurrection"
    );
}

/// Test plan #3 second bullet — `WIBinder::upgrade()` `Err(DeadObject)`
/// branch coverage on the in-tree death-recipient path.
///
/// Pre-PR `WIBinder` held a strong `Arc<dyn IBinder>`, so
/// `WIBinder::upgrade()` always succeeded — the `Err` branch was
/// dead code. Under this PR `WIBinder` is `sync::Weak<dyn IBinder>`,
/// and once the last `Arc<ProxyHandle>` is dropped (which happens
/// after `BR_DEAD_BINDER` removes the cache entry and any user-held
/// Strong drops) `upgrade()` legitimately returns `Err(DeadObject)`.
///
/// Marked `#[ignore]` because it kills the shared `test_service`,
/// which would break tests running in parallel. Run explicitly:
///
/// ```text
/// cargo test --package tests test_wibinder_upgrade_after_obituary -- --ignored
/// ```
#[test]
#[ignore]
fn test_wibinder_upgrade_after_obituary() {
    init_test();

    let test_service = get_test_service();
    let weak: WIBinder = SIBinder::downgrade(&test_service.as_binder());

    // Upgrade succeeds while the proxy is live.
    let upgraded = weak.upgrade().expect("upgrade must succeed pre-obituary");
    drop(upgraded);

    // Set up a death recipient so we can wait for the obituary to
    // propagate.
    let (mut read_file, write_file) = build_pipe();
    struct DR(Mutex<File>);
    impl DeathRecipient for DR {
        fn binder_died(&self, _: &WIBinder) {
            self.0
                .lock()
                .unwrap()
                .write_all(b"died\n")
                .expect("write to pipe");
        }
    }
    let recipient: Arc<dyn DeathRecipient> = Arc::new(DR(Mutex::new(write_file)));
    test_service
        .as_binder()
        .link_to_death(Arc::downgrade(&recipient))
        .expect("link_to_death must succeed");

    test_service
        .killService()
        .expect("killService must succeed");

    // Wait for the death notification. We cannot use `read_to_string`
    // here: `recipient` (and therefore `write_file` inside its Mutex)
    // stays alive until the end of this function, so the pipe's write
    // end never closes and `read_to_string` would block forever
    // waiting for EOF. Read exactly the 5 bytes the recipient wrote
    // ("died\n") with `read_exact`, which returns as soon as the data
    // is available without needing the writer side to close.
    let mut buf = [0u8; 5];
    read_file.read_exact(&mut buf).expect("read death pipe");
    assert_eq!(&buf, b"died\n");

    // Now we can drop `recipient` so the pipe writer side closes —
    // not strictly required for the assertion below, but cleaner.
    drop(recipient);

    // Drop our last Strong<dyn ITestService>; the obituary already
    // removed the cache entry, so the underlying `Arc<ProxyHandle>`
    // is now the only thing keeping the inner Arc alive. After this
    // drop it goes to 0.
    drop(test_service);

    // `upgrade()` MUST return Err(DeadObject) now. Pre-PR this
    // assertion would not have been reachable (upgrade always Ok).
    match weak.upgrade() {
        Err(rsbinder::StatusCode::DeadObject) => {}
        Err(other) => panic!("expected DeadObject, got {other:?}"),
        Ok(_) => {
            panic!("regression: WIBinder::upgrade() succeeded after obituary + last Strong drop")
        }
    }
}

// =============================================================================
// FOLLOW_UP_PR_104 integration tests
//
// These exercise the death-notification / extension / dump fixes through real
// binder communication against `test_service`, complementing the in-crate unit
// tests in `rsbinder/src/proxy.rs` and `rsbinder/src/thread_state.rs`. The
// `#[ignore]`'d tests destroy the live `test_service` via `killService()` and
// must each be run after a service restart — see the integration-test CI
// workflow for the restart-then-run pattern.
//
// Coverage notes for items NOT exercised here:
//   - Item 5  (BR_DEAD_BINDER kernel handshake)         — needs driver-level
//     fault injection that rsbinder has no harness for.
//     Practical bar: the orchestration unit test in `thread_state.rs`.
//   - Item 10 (`Transactable::transact` panic isolation) — would need a
//     deliberately panicking `Transactable` registered as a service. Out of
//     scope without modifying `test_service` or `ITestService.aidl`.
//     Practical bar: the unit test in `thread_state.rs` exercising the
//     same `dispatch_transact_caught` code path that the BR_TRANSACTION arm
//     calls in production.
//   - Item 11 (extension cache staleness)               — would need an
//     extension that can die independently of its parent service, which the
//     test_service's `NamedCallback` extension does not support.
//     Practical bar: the unit test in `proxy.rs` that pre-populates the cache.
// =============================================================================

/// Item 6: a panicking `DeathRecipient` must not starve other recipients
/// registered against the same handle, and must not terminate the binder
/// worker thread. Two recipients are registered: a panicking one (which
/// goes first in registration order) and a writing one. After `killService`
/// the writing recipient's pipe-write must be observable — proof that
/// `dispatch_obituary_callbacks`'s `catch_unwind` is in effect.
#[test]
#[ignore]
fn test_death_recipient_panic_does_not_starve_others() {
    init_test();
    let test_service = get_test_service();
    let (mut read_file, write_file) = build_pipe();

    struct PanickingRecipient;
    impl DeathRecipient for PanickingRecipient {
        fn binder_died(&self, _: &WIBinder) {
            panic!("intentional test panic in binder_died");
        }
    }

    struct WritingRecipient(Mutex<File>);
    impl DeathRecipient for WritingRecipient {
        fn binder_died(&self, _: &WIBinder) {
            self.0
                .lock()
                .unwrap()
                .write_all(b"survived\n")
                .expect("pipe write");
        }
    }

    let panic_arc: Arc<dyn DeathRecipient> = Arc::new(PanickingRecipient);
    let writing_arc: Arc<dyn DeathRecipient> = Arc::new(WritingRecipient(Mutex::new(write_file)));

    let binder = test_service.as_binder();
    // Order matters: the panicker registers first. A regression that
    // drops the per-recipient `catch_unwind` would unwind through the
    // dispatch loop and the writing recipient would never fire — the
    // `read_exact` below would then block until pipe close (Mutex<File>
    // dropping at end of test) and surface a different failure mode.
    binder
        .link_to_death(Arc::downgrade(&panic_arc))
        .expect("link panicking recipient");
    binder
        .link_to_death(Arc::downgrade(&writing_arc))
        .expect("link writing recipient");

    test_service.killService().expect("killService");

    let mut buf = [0u8; 9];
    read_file
        .read_exact(&mut buf)
        .expect("writing recipient must fire despite panicking sibling");
    assert_eq!(&buf, b"survived\n");

    // Hold both recipients alive until the assertion above runs so
    // their `Weak`s in the recipients vec stayed upgradable for the
    // obituary dispatch.
    drop(panic_arc);
    drop(writing_arc);
}

/// Item 1: registering the same recipient twice and unlinking once must
/// remove only one entry (matching C++ `BpBinder::unlinkToDeath`'s
/// `removeAt(i)`). After `killService` exactly one `binder_died` call
/// must fire on the duplicated recipient.
///
/// The recipients vec ends up `[counted, counted, signal]` (three
/// `link_to_death` calls in registration order). `unlink_to_death`'s
/// position-based first-match remove is then expected to leave
/// `[counted, signal]`. The signal recipient is the synchronization
/// barrier: dispatch fires recipients in vec order, so the signal
/// byte arrives **after** every preceding `counted` invocation has
/// returned. Reading the signal therefore guarantees the atomic
/// counter has its final value — a pipe-only design risks a
/// false positive where the test reads one byte, drops the recipient,
/// and silently masks a still-pending second `binder_died` call (its
/// `weak.upgrade()` would return None after the drop).
///
/// Failure modes a regression would cause:
/// - `Vec::retain` (pre-Item-1): both `counted` entries removed →
///   counter == 0 → assert fails.
/// - No removal at all: vec stays `[counted, counted, signal]` →
///   counter == 2 → assert fails.
#[test]
#[ignore]
fn test_unlink_to_death_single_remove_via_obituary() {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    init_test();
    let test_service = get_test_service();

    struct CountingRecipient(Arc<AtomicU32>);
    impl DeathRecipient for CountingRecipient {
        fn binder_died(&self, _: &WIBinder) {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }
    let counter = Arc::new(AtomicU32::new(0));
    let counted: Arc<dyn DeathRecipient> = Arc::new(CountingRecipient(counter.clone()));

    let (mut signal_read, signal_write) = build_pipe();
    struct SignalRecipient(Mutex<File>);
    impl DeathRecipient for SignalRecipient {
        fn binder_died(&self, _: &WIBinder) {
            self.0
                .lock()
                .unwrap()
                .write_all(b"!")
                .expect("signal pipe write");
        }
    }
    let signal: Arc<dyn DeathRecipient> = Arc::new(SignalRecipient(Mutex::new(signal_write)));

    let binder = test_service.as_binder();
    // recipients vec after these three calls: [counted, counted, signal]
    binder
        .link_to_death(Arc::downgrade(&counted))
        .expect("link counted (1st)");
    binder
        .link_to_death(Arc::downgrade(&counted))
        .expect("link counted (2nd duplicate)");
    binder
        .link_to_death(Arc::downgrade(&signal))
        .expect("link signal");

    // Single-position remove of the first match → [counted, signal].
    binder
        .unlink_to_death(Arc::downgrade(&counted))
        .expect("unlink one of the duplicates");

    test_service.killService().expect("killService");

    // Wait for the signal byte. Because `signal` is last in the
    // recipients vec, by the time we observe its byte the binder
    // thread has already returned from every preceding
    // `counted.binder_died` invocation.
    let mut sentinel = [0u8; 1];
    signal_read
        .read_exact(&mut sentinel)
        .expect("read signal pipe");
    assert_eq!(&sentinel, b"!");

    let count = counter.load(AtomicOrdering::SeqCst);
    assert_eq!(
        count, 1,
        "exactly one binder_died expected for the duplicated recipient (got {count}). \
         Vec::retain regression would yield 0; missed-removal regression would yield 2."
    );

    drop(counted);
    drop(signal);
}

/// Item 8: passing an already-dead `Weak<dyn DeathRecipient>` to
/// `link_to_death` against a live remote proxy must reject with
/// `BadValue`, **not** silently register a subscription that
/// `binder_died` would skip. Non-destructive — does not call
/// `killService`.
#[test]
fn test_link_to_death_rejects_already_dead_weak_remote_proxy() {
    init_test();
    let test_service = get_test_service();

    struct NoopRecipient;
    impl DeathRecipient for NoopRecipient {
        fn binder_died(&self, _: &WIBinder) {}
    }

    let arc: Arc<dyn DeathRecipient> = Arc::new(NoopRecipient);
    let dead_weak = Arc::downgrade(&arc);
    drop(arc);
    assert!(
        dead_weak.upgrade().is_none(),
        "fixture sanity: weak must be dangling"
    );

    let result = test_service.as_binder().link_to_death(dead_weak);
    assert_eq!(
        result,
        Err(rsbinder::StatusCode::BadValue),
        "link_to_death must reject a dead weak with BadValue (got {result:?})"
    );
}

/// Item 3: `set_extension` on a remote proxy must reject with
/// `InvalidOperation` — the operation is server-side only and a proxy
/// has no way to inform the remote service. The post-PR-104 §4
/// strong-cache common case would otherwise silently pin an unrelated
/// `Arc<dyn IBinder>` for the parent's lifetime. Non-destructive —
/// `get_extension` continues to work, returning the extension that
/// `test_service` set on its native side.
#[test]
fn test_set_extension_on_remote_proxy_rejects() {
    init_test();
    let test_service = get_test_service();
    let binder = test_service.as_binder();

    assert!(
        binder.is_remote(),
        "test_service.as_binder() must be a remote proxy for this test"
    );

    let new_ext = INamedCallback::BnNamedCallback::new_binder(ExtNamedCallback("rejected".into()));
    let result = binder.set_extension(&new_ext.as_binder());
    assert_eq!(
        result,
        Err(rsbinder::StatusCode::InvalidOperation),
        "set_extension on a remote proxy must reject (got {result:?})"
    );

    // The proxy is still usable — `get_extension` returns the
    // extension `test_service` set on its native side.
    let got = binder
        .get_extension()
        .expect("get_extension must still succeed after rejected set_extension");
    assert!(
        got.is_some(),
        "test_service publishes a NamedCallback extension"
    );
}

/// Item 4: `dump` on an obituary'd proxy must return `DeadObject`
/// and the caller's fd must end up closed.
///
/// Scope of this integration test: it verifies the **user-visible
/// contract** (DeadObject return + fd no longer open) end-to-end
/// against a real obituary'd proxy. It does **not** distinguish
/// between Item 4's specific fast-fail path (which closes the fd
/// via `File::Drop` *before* any parcel work) and the pre-existing
/// `Parcel::Drop` cleanup path (which would close via
/// `release_objects` *after* `into_raw_fd` and the parcel write,
/// were Item 4's check ever removed and `submit_transact`'s own
/// fast-fail relied upon instead). Both layers result in the same
/// observable EOF on the read end.
///
/// The unit test
/// `test_dump_fast_fails_and_drops_fd_when_obituary_sent` in
/// `rsbinder/src/proxy.rs` covers Item 4's specific path with a
/// synthetic `IntoRawFd` type whose `Drop` is observable separately
/// from `into_raw_fd` — that test catches a regression that
/// reverts Item 4's check while leaving `submit_transact`'s
/// PR-#104 fast-fail in place. This integration test catches the
/// broader regression where neither layer closes the fd
/// (e.g. both fast-fail checks reverted).
#[test]
#[ignore]
fn test_dump_fast_fails_on_dead_proxy_closes_fd() {
    init_test();
    let test_service = get_test_service();
    let binder = test_service.as_binder();

    // Wait for obituary by registering a death recipient + pipe.
    let (mut death_read, death_write) = build_pipe();
    struct DR(Mutex<File>);
    impl DeathRecipient for DR {
        fn binder_died(&self, _: &WIBinder) {
            self.0.lock().unwrap().write_all(b"died\n").unwrap();
        }
    }
    let recipient: Arc<dyn DeathRecipient> = Arc::new(DR(Mutex::new(death_write)));
    binder
        .link_to_death(Arc::downgrade(&recipient))
        .expect("link_to_death");

    test_service.killService().expect("killService");

    let mut sentinel = [0u8; 5];
    death_read
        .read_exact(&mut sentinel)
        .expect("read death pipe");
    assert_eq!(&sentinel, b"died\n");

    // Now `obituary_sent` is published on the proxy. Build a fresh
    // pipe; the write end is handed to `dump`, the read end stays in
    // the test for EOF observation.
    let (dump_read_owned, dump_write_owned) = rustix::pipe::pipe().expect("pipe for dump");
    let dump_write = unsafe { File::from_raw_fd(dump_write_owned.into_raw_fd()) };
    let mut dump_read = unsafe { File::from_raw_fd(dump_read_owned.into_raw_fd()) };

    // `dump` is a `ProxyHandle` inherent method (not on the `IBinder`
    // trait), so we go through `as_proxy` to reach it. Holding the
    // SIBinder root in `binder` keeps the proxy reference valid.
    let proxy = (*binder)
        .as_proxy()
        .expect("test_service is a remote proxy");
    let result = proxy.dump(dump_write, &[]);
    assert_eq!(
        result,
        Err(rsbinder::StatusCode::DeadObject),
        "dump on obituary'd proxy must fast-fail with DeadObject (got {result:?})"
    );

    // `dump` dropped its `F` parameter without calling `into_raw_fd`,
    // so File::Drop closed the fd. Reading from the other end must
    // EOF — pipe semantics guarantee EOF on read once all write ends
    // are closed.
    let mut tail = [0u8; 16];
    let n = dump_read
        .read(&mut tail)
        .expect("read dump pipe after fast-fail");
    assert_eq!(
        n, 0,
        "dump's fd must have been closed by RAII (got {n} unexpected bytes)"
    );

    drop(recipient);
}

/// Soak the publish-native → drop → kernel-deref cycle that the new
/// id-encoded `flat_binder_object` lifecycle replaces. Under the prior
/// fat-pointer encoding, this loop could trigger UAF when `Inner<T>`
/// was dropped between `BR_RELEASE` and `BR_DECREFS` (the deferred
/// `BR_DECREFS` arm reconstructed an SIBinder via `from_raw` and
/// dispatched `dec_weak` on a freed allocation).
///
/// Each iteration:
///   1. Constructs a fresh native binder (`BnNamedCallback::new_binder`
///      over a local `ExtNamedCallback` impl).
///   2. Publishes it across the kernel via `service.TakesAnIBinder` —
///      our process emits `flat_binder_object { binder = id, cookie =
///      0 }` for `BINDER_TYPE_BINDER`; the remote receives it as a
///      handle, holds it for the call duration, then drops it. The
///      kernel then schedules `BR_RELEASE` / `BR_DECREFS` back to us.
///   3. Drops every user-side strong reference to the local binder.
///      Under the new model the sidecar `published_natives` table
///      keeps the canonical `Arc<dyn IBinder>` strong while
///      `kernel_refs > 0`; entry removal — and therefore
///      `Inner<T>::drop` — only happens once both `publish_count` and
///      `kernel_refs` reach zero.
///
/// 100 iterations is the same soak depth used by
/// `integration-test.yml`. A regression here surfaces either as a
/// segfault (UAF) or as kernel-driver `EINVAL` on `BC_FREE_BUFFER`
/// (table entry torn down before the kernel finished its
/// dereferences).
#[test]
fn test_native_publish_drop_release_cycle() {
    let service = get_test_service();

    for i in 0..100 {
        let local =
            INamedCallback::BnNamedCallback::new_binder(ExtNamedCallback(format!("uaf_soak_{i}")));
        let binder = local.as_binder();
        // Fire-and-forget: the remote receives, holds briefly, then
        // releases. We only care that the round-trip completes
        // without crashing or returning an error.
        service
            .TakesAnIBinder(&binder)
            .unwrap_or_else(|err| panic!("TakesAnIBinder iter {i} failed: {err:?}"));

        // Drop both strong handles. The table's `binder_pin` keeps
        // the inner Arc alive while the kernel still has refs.
        drop(binder);
        drop(local);
    }
}
