#![allow(non_snake_case, dead_code, unused_imports, unused_macros)]

use env_logger::Env;

pub use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/test_aidl.rs"));

use android::aidl::fixedsizearray::FixedSizeArrayExample::{
    FixedSizeArrayExample,
    IRepeatFixedSizeArray::{BpRepeatFixedSizeArray, IRepeatFixedSizeArray},
    IntParcelable::IntParcelable,
};
use android::aidl::tests::nested::{
    INestedService, ParcelableWithNested,
};
use android::aidl::tests::unions::EnumUnion::EnumUnion;
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
use android::aidl::tests::nonvintf::{
    NonVintfExtendableParcelable::NonVintfExtendableParcelable,
    NonVintfParcelable::NonVintfParcelable,
};
use android::aidl::tests::unstable::{
    UnstableExtendableParcelable::UnstableExtendableParcelable,
    UnstableParcelable::UnstableParcelable,
};
use android::aidl::versioned::tests::{
    BazUnion::BazUnion, Foo::Foo, IFooInterface, IFooInterface::BpFooInterface,
};
use android::aidl::tests::vintf::{
    VintfExtendableParcelable::VintfExtendableParcelable, VintfParcelable::VintfParcelable,
};
use rustix::fd::OwnedFd;
use std::{fs::File, os::fd::IntoRawFd};
use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::sync::{Arc, Mutex};

fn init_logger() {
    let _ = env_logger::Builder::from_env(Env::default().default_filter_or("debug")).try_init();
}

fn init_test() {
    init_logger();
    ProcessState::init_default();
}

fn get_test_service() -> rsbinder::Strong<dyn ITestService::ITestService> {
    init_test();
    hub::get_interface(<BpTestService as ITestService::ITestService>::descriptor())
        .unwrap_or_else(|_| panic!("did not get binder service: {}",
            <BpTestService as ITestService::ITestService>::descriptor()))
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

test_reverse_array! {test_array_boolean, ReverseBoolean, vec![true, false, false]}
test_reverse_array! {test_array_byte, ReverseByte, vec![255u8, 0u8, 127u8]}
test_reverse_array! {
    service,
    ReverseChar,
    vec!['A' as u16, 'B' as u16, 'C' as u16]
}
test_reverse_array! {test_array_int, ReverseInt, vec![1, 2, 3]}
test_reverse_array! {test_array_long, ReverseLong, vec![-1i64, 0i64, 1i64 << 60]}
test_reverse_array! {test_array_float, ReverseFloat, vec![-0.3f32, -0.7f32, 8.0f32]}
test_reverse_array! {
    test_array_double,
    ReverseDouble,
    vec![1.0f64 / 3.0f64, 1.0f64 / 7.0f64, 42.0f64]
}
test_reverse_array! {
    test_array_string,
    ReverseString,
    vec!["f".into(), "a".into(), "b".into()]
}
test_reverse_array! {
    test_array_byte_enum,
    ReverseByteEnum,
    vec![ByteEnum::FOO, ByteEnum::BAR, ByteEnum::BAR]
}
test_reverse_array! {
    test_array_byte_enum_values,
    ReverseByteEnum,
    ByteEnum::enum_values()
}
test_reverse_array! {
    test_array_byte_enum_v2,
    ReverseByteEnum,
    vec![ByteEnum::FOO, ByteEnum::BAR, ByteEnum::BAZ]
}
test_reverse_array! {
    test_array_int_enum,
    ReverseIntEnum,
    vec![IntEnum::FOO, IntEnum::BAR, IntEnum::BAR]
}
test_reverse_array! {
    test_array_long_enum,
    ReverseLongEnum,
    vec![LongEnum::FOO, LongEnum::BAR, LongEnum::BAR]
}
test_reverse_array! {
    test_array_string_list,
    ReverseStringList,
    vec!["f".into(), "a".into(), "b".into()]
}
test_reverse_array! {
    test_array_utf8_string,
    ReverseUtf8CppString,
    vec![
        "a".into(),
        String::new(),
        std::str::from_utf8(&[0xC3, 0xB8])
            .expect("error converting string")
            .into(),
    ]
}

#[test]
fn test_binder_exchange() {
    const NAME: &str = "Smythe";
    let service = get_test_service();
    let got = service.GetOtherTestService(NAME).expect("error calling GetOtherTestService");
    assert_eq!(got.GetName().as_ref().map(String::as_ref), Ok(NAME));
    assert_eq!(service.VerifyName(&got, NAME), Ok(true));
}

#[test]
fn test_binder_array_exchange() {
    let names = vec!["Fizz".into(), "Buzz".into()];
    let service = get_test_service();
    let got = service.GetInterfaceArray(&names).expect("error calling GetInterfaceArray");
    assert_eq!(got.iter().map(|s| s.GetName()).collect::<std::result::Result<Vec<_>, _>>(), Ok(names.clone()));
    assert_eq!(service.VerifyNamesWithInterfaceArray(&got, &names), Ok(true));
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
            .map(|opt_s| opt_s.as_ref().map(|s| s.GetName().expect("error calling GetName")))
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
    let got = service.GetInterfaceList(Some(&names)).expect("error calling GetInterfaceList");
    assert_eq!(
        got.as_ref().map(|arr| arr
            .iter()
            .map(|opt_s| opt_s.as_ref().map(|s| s.GetName().expect("error calling GetName")))
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
    unsafe { (File::from_raw_fd(fds.0.into_raw_fd()), File::from_raw_fd(fds.1.into_raw_fd())) }
}

/// Helper function that constructs a `File` from a `ParcelFileDescriptor`.
///
/// This is needed because `File` is currently the way to read and write
/// to pipes using the `Read` and `Write` traits.
fn file_from_pfd(fd: &rsbinder::ParcelFileDescriptor) -> File {
    fd.as_ref().try_clone().expect("failed to clone file descriptor").into()
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
    file_from_pfd(&result_pfd).write_all(TEST_DATA).expect("error writing to pipe");

    let mut buf = [0u8; TEST_DATA.len()];
    read_file.read_exact(&mut buf).expect("error reading from pipe");
    assert_eq!(&buf[..], TEST_DATA);
}

#[test]
fn test_parcel_file_descriptor_array() {
    let service = get_test_service();

    let (read_file, write_file) = build_pipe();
    let input = vec![
        rsbinder::ParcelFileDescriptor::new(read_file),
        rsbinder::ParcelFileDescriptor::new(write_file),
    ];

    let mut repeated = vec![];

    let backend = service.getBackendType().expect("error getting backend type");
    if backend == BackendType::JAVA {
        // Java needs initial values here (can't resize arrays)
        // Other backends can't accept 'None', but we can use it in Java for convenience, rather
        // than creating file descriptors.
        repeated = vec![None, None];
    }

    let result = service
        .ReverseParcelFileDescriptorArray(&input[..], &mut repeated)
        .expect("error calling ReverseParcelFileDescriptorArray");

    file_from_pfd(&input[1]).write_all(b"First").expect("error writing to pipe");
    file_from_pfd(repeated[1].as_ref().expect("received None for ParcelFileDescriptor"))
        .write_all(b"Second")
        .expect("error writing to pipe");
    file_from_pfd(&result[0]).write_all(b"Third").expect("error writing to pipe");

    const TEST_DATA: &[u8] = b"FirstSecondThird";
    let mut buf = [0u8; TEST_DATA.len()];
    file_from_pfd(&input[0]).read_exact(&mut buf).expect("error reading from pipe");
    assert_eq!(&buf[..], TEST_DATA);
}

#[test]
fn test_service_specific_exception() {
    let service = get_test_service();

    for i in -1..2 {
        let result: std::prelude::v1::Result<(), Status> = service.ThrowServiceException(i);
        assert!(result.is_err());

        let status = result.unwrap_err();
        assert_eq!(status.exception_code(), rsbinder::ExceptionCode::ServiceSpecific);
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
    assert!(service.GetCallback(true).expect("error calling GetCallback").is_none());
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
    let backend = service.getBackendType().expect("error getting backend type");
    let null_output: Option<Vec<Option<String>>> = if backend == BackendType::JAVA { Some(vec![]) } else { None };
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

    assert_eq!(parcelable.shouldContainThreeFs, [DESIRED_VALUE, DESIRED_VALUE, DESIRED_VALUE]);
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
    assert_eq!(parcelable.addString2, "The quick brown fox jumps over the lazy dog.");

    assert_eq!(
        parcelable.shouldSetBit0AndBit2,
        StructuredParcelable::BIT0 | StructuredParcelable::BIT2
    );

    assert_eq!(parcelable.u, Some(Union::Union::Ns(vec![1, 2, 3])));
    assert_eq!(parcelable.shouldBeConstS1, Some(Union::Union::S(Union::S1.to_string())))
}

#[test]
fn test_repeat_extendable_parcelable() {
    let service = get_test_service();

    let ext = Arc::new(MyExt { a: 42, b: "EXT".into() });
    let mut ep = ExtendableParcelable { a: 1, b: "a".into(), c: 42, ..Default::default() };
    ep.ext.set_parcelable(Arc::clone(&ext)).expect("error setting parcelable");

    let mut ep2 = ExtendableParcelable::default();
    let result: std::prelude::v1::Result<(), Status> = service.RepeatExtendableParcelable(&ep, &mut ep2);
    assert_eq!(result, Ok(()));
    assert_eq!(ep2.a, ep.a);
    assert_eq!(ep2.b, ep.b);

    let ret_ext = ep2.ext.get_parcelable::<MyExt>().expect("error getting parcelable");
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

// #[test]
// fn test_vintf_parcelable_holder_cannot_contain_not_vintf_parcelable() {
//     let mut holder = VintfExtendableParcelable::default();
//     let parcelable = Arc::new(NonVintfParcelable::default());
//     let result = holder.ext.set_parcelable(Arc::clone(&parcelable));
//     assert_eq!(result, Err(rsbinder::StatusCode::BadValue));

//     let parcelable2 = holder.ext.get_parcelable::<NonVintfParcelable>();
//     assert!(parcelable2.unwrap().is_none());
// }

// #[test]
// fn test_vintf_parcelable_holder_cannot_contain_unstable_parcelable() {
//     let mut holder = VintfExtendableParcelable::default();
//     let parcelable = Arc::new(UnstableParcelable::default());
//     let result = holder.ext.set_parcelable(Arc::clone(&parcelable));
//     assert_eq!(result, Err(rsbinder::StatusCode::BadValue));

//     let parcelable2 = holder.ext.get_parcelable::<UnstableParcelable>();
//     assert!(parcelable2.unwrap().is_none());
// }

#[test]
fn test_read_write_extension() {
    let ext = Arc::new(MyExt { a: 42, b: "EXT".into() });
    let ext2 = Arc::new(MyExt2 { a: 42, b: MyExt { a: 24, b: "INEXT".into() }, c: "EXT2".into() });

    let mut ep = ExtendableParcelable { a: 1, b: "a".into(), c: 42, ..Default::default() };

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

    let new_ext2 =
        Arc::new(MyExt2 { a: 79, b: MyExt { a: 42, b: "INNEWEXT".into() }, c: "NEWEXT2".into() });
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
        let node = RecursiveList { value: n, next: head };
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
    let di: ITestServiceDefaultRef = Some(Arc::new(TestDefaultImpl));
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

    assert_eq!(service.acceptUnionAndReturnString(&BazUnion::IntNum(42)), Ok(String::from("42")));
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

// #[test]
// fn test_renamed_interface_new_as_new() {
//     test_renamed_interface(|_, new_name| {
//         assert_eq!(
//             <BpNewName as INewName::INewName>::descriptor(),
//             "android.aidl.tests.IOldName"
//         );

//         let real_name = new_name.RealName();
//         assert_eq!(real_name.as_ref().map(String::as_str), Ok("NewName"));
//     });
// }

// #[test]
// fn test_renamed_interface_old_as_new() {
//     test_renamed_interface(|old_name, _| {
//         let new_name = old_name.as_binder().into_interface::<dyn INewName::INewName>();
//         assert!(new_name.is_ok());

//         let real_name = new_name.unwrap().RealName();
//         assert_eq!(real_name.as_ref().map(String::as_str), Ok("OldName"));
//     });
// }

// #[test]
// fn test_renamed_interface_new_as_old() {
//     test_renamed_interface(|_, new_name| {
//         let old_name = new_name.as_binder().into_interface::<dyn IOldName::IOldName>();
//         assert!(old_name.is_ok());

//         let real_name = old_name.unwrap().RealName();
//         assert_eq!(real_name.as_ref().map(String::as_str), Ok("NewName"));
//     });
// }

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
        Ok(INestedService::Result::Result { status: ParcelableWithNested::Status::Status::NOT_OK })
    );
    let received = Arc::new(Mutex::new(None));
    // NOT_OK -> OK with nested callback interface
    let cb = INestedService::ICallback::BnCallback::new_binder(
        Callback { received: Arc::clone(&received) }
    );
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
    assert_eq!(repeated.into_iter().collect::<Option<Vec<_>>>().as_ref(), Some(&array));
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
    p.stringNullableMatrix =
        Some([[Some("hello".into()), Some("world".into())], Default::default()]);

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
    test_repeat_fixed_size_array!(service, Repeat2dInts, [[1i32, 2i32, 3i32], [1i32, 2i32, 3i32]]);

    test_repeat_fixed_size_array_2d_binder!(
        service,
        Repeat2dBinders,
        [[binder1.clone(), binder2.clone(), binder3.clone()], [binder1, binder2, binder3]]
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

    test_service.as_binder().as_proxy().unwrap().dump(write_file, &args).unwrap();
    let mut buf = String::new();
    read_file.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, expected);
}
