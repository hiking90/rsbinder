#![allow(non_snake_case)]

use env_logger::Env;

use std::collections::HashMap;
use std::sync::Mutex;

pub use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/test_aidl.rs"));

pub use android::aidl::fixedsizearray::FixedSizeArrayExample::{
    IRepeatFixedSizeArray, IntParcelable::IntParcelable,
};

pub use android::aidl::tests::nested::{
    INestedService, ParcelableWithNested,
};
pub use android::aidl::tests::ITestService::{
    self, BnTestService, BpTestService, Empty::Empty,
};
pub use android::aidl::tests::{
    extension::ExtendableParcelable::ExtendableParcelable, extension::MyExt::MyExt,
    BackendType::BackendType, ByteEnum::ByteEnum, ConstantExpressionEnum::ConstantExpressionEnum,
    INamedCallback, INewName, IOldName, IntEnum::IntEnum, LongEnum::LongEnum,
    RecursiveList::RecursiveList, StructuredParcelable, Union,
};
pub use android::aidl::versioned::tests::{
    BazUnion::BazUnion, Foo::Foo, IFooInterface, IFooInterface::BnFooInterface,
    IFooInterface::BpFooInterface,
};

fn dup_fd(fd: &ParcelFileDescriptor) -> ParcelFileDescriptor {
    // ParcelFileDescriptor::new(fd.as_ref().try_clone().unwrap())
    todo!()
}

struct NamedCallback(String);

impl rsbinder::Interface for NamedCallback {}

impl INamedCallback::INamedCallback for NamedCallback {
    fn GetName(&self) -> std::result::Result<String, Status> {
        Ok(self.0.clone())
    }
}

struct OldName;

impl rsbinder::Interface for OldName {}

impl IOldName::IOldName for OldName {
    fn RealName(&self) -> std::result::Result<String, Status> {
        Ok("OldName".into())
    }
}

#[derive(Debug, Default)]
struct NewName;

impl rsbinder::Interface for NewName {}

impl INewName::INewName for NewName {
    fn RealName(&self) -> std::result::Result<String, Status> {
        Ok("NewName".into())
    }
}


#[derive(Default)]
struct TestService {
    service_map: Mutex<HashMap<String, rsbinder::Strong<dyn INamedCallback::INamedCallback>>>,
}

impl Interface for TestService {}

macro_rules! impl_repeat {
    ($repeat_name:ident, $type:ty) => {
        fn $repeat_name(&self, token: $type) -> std::result::Result<$type, rsbinder::Status> {
            Ok(token)
        }
    };
}

macro_rules! impl_reverse {
    ($reverse_name:ident, $type:ty) => {
        fn $reverse_name(
            &self,
            input: &[$type],
            repeated: &mut Vec<$type>,
        ) -> std::result::Result<Vec<$type>, Status> {
            repeated.clear();
            repeated.extend_from_slice(input);
            Ok(input.iter().rev().cloned().collect())
        }
    };
}

macro_rules! impl_repeat_reverse {
    ($repeat_name:ident, $reverse_name:ident, $type:ty) => {
        impl_repeat! {$repeat_name, $type}
        impl_reverse! {$reverse_name, $type}
    };
}

macro_rules! impl_repeat_nullable {
    ($repeat_nullable_name:ident, $type:ty) => {
        fn $repeat_nullable_name(
            &self,
            input: Option<&[$type]>,
        ) -> std::result::Result<Option<Vec<$type>>, rsbinder::Status> {
            Ok(input.map(<[$type]>::to_vec))
        }
    };
}

impl ITestService::ITestService for TestService {
    impl_repeat! {RepeatByte, i8}
    impl_reverse! {ReverseByte, u8}

    fn UnimplementedMethod(&self, _: i32) -> std::result::Result<i32, rsbinder::Status> {
        // Pretend this method hasn't been implemented
        Err(rsbinder::StatusCode::UnknownTransaction.into())
    }

    fn TestOneway(&self) -> std::result::Result<(), rsbinder::Status> {
        Err(rsbinder::StatusCode::Unknown.into())
    }

    fn Deprecated(&self) -> std::result::Result<(), rsbinder::Status> {
        Ok(())
    }

    impl_repeat_reverse! {RepeatBoolean, ReverseBoolean, bool}
    impl_repeat_reverse! {RepeatChar, ReverseChar, u16}
    impl_repeat_reverse! {RepeatInt, ReverseInt, i32}
    impl_repeat_reverse! {RepeatLong, ReverseLong, i64}
    impl_repeat_reverse! {RepeatFloat, ReverseFloat, f32}
    impl_repeat_reverse! {RepeatDouble, ReverseDouble, f64}
    impl_repeat_reverse! {RepeatByteEnum, ReverseByteEnum, ByteEnum}
    impl_repeat_reverse! {RepeatIntEnum, ReverseIntEnum, IntEnum}
    impl_repeat_reverse! {RepeatLongEnum, ReverseLongEnum, LongEnum}
    impl_reverse! {ReverseString, String}
    impl_reverse! {ReverseStringList, String}
    impl_reverse! {ReverseUtf8CppString, String}

    fn RepeatString(&self, input: &str) -> std::result::Result<String, rsbinder::Status> {
        Ok(input.into())
    }

    fn RepeatUtf8CppString(&self, input: &str) -> std::result::Result<String, rsbinder::Status> {
        Ok(input.into())
    }

    fn GetOtherTestService(
        &self,
        name: &str,
    ) -> std::result::Result<rsbinder::Strong<dyn INamedCallback::INamedCallback>, rsbinder::Status> {
        let mut service_map = self.service_map.lock().unwrap();
        let other_service = service_map.entry(name.into()).or_insert_with(|| {
            let named_callback = NamedCallback(name.into());
            INamedCallback::BnNamedCallback::new_binder(named_callback)
        });
        Ok(other_service.to_owned())
    }

    fn VerifyName(
        &self,
        service: &rsbinder::Strong<dyn INamedCallback::INamedCallback>,
        name: &str,
    ) -> std::result::Result<bool, rsbinder::Status> {
        service.GetName().map(|found_name| found_name == name)
    }

    fn GetInterfaceArray(
        &self,
        names: &[String],
    ) -> std::result::Result<Vec<rsbinder::Strong<dyn INamedCallback::INamedCallback>>, rsbinder::Status> {
        names.iter().map(|name| self.GetOtherTestService(name)).collect()
    }

    fn VerifyNamesWithInterfaceArray(
        &self,
        services: &[rsbinder::Strong<dyn INamedCallback::INamedCallback>],
        names: &[String],
    ) -> std::result::Result<bool, rsbinder::Status> {
        if services.len() == names.len() {
            for (s, n) in services.iter().zip(names) {
                if !self.VerifyName(s, n)? {
                    return Ok(false);
                }
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn GetNullableInterfaceArray(
        &self,
        names: Option<&[Option<String>]>,
    ) -> std::result::Result<Option<Vec<Option<rsbinder::Strong<dyn INamedCallback::INamedCallback>>>>, Status>
    {
        if let Some(names) = names {
            let mut services = vec![];
            for name in names {
                if let Some(name) = name {
                    services.push(Some(self.GetOtherTestService(name)?));
                } else {
                    services.push(None);
                }
            }
            Ok(Some(services))
        } else {
            Ok(None)
        }
    }

    fn VerifyNamesWithNullableInterfaceArray(
        &self,
        services: Option<&[Option<rsbinder::Strong<dyn INamedCallback::INamedCallback>>]>,
        names: Option<&[Option<String>]>,
    ) -> std::result::Result<bool, rsbinder::Status> {
        if let (Some(services), Some(names)) = (services, names) {
            for (s, n) in services.iter().zip(names) {
                if let (Some(s), Some(n)) = (s, n) {
                    if !self.VerifyName(s, n)? {
                        return Ok(false);
                    }
                } else if s.is_some() || n.is_some() {
                    return Ok(false);
                }
            }
            Ok(true)
        } else {
            Ok(services.is_none() && names.is_none())
        }
    }

    fn GetInterfaceList(
        &self,
        names: Option<&[Option<String>]>,
    ) -> std::result::Result<Option<Vec<Option<rsbinder::Strong<dyn INamedCallback::INamedCallback>>>>, Status>
    {
        self.GetNullableInterfaceArray(names)
    }

    fn VerifyNamesWithInterfaceList(
        &self,
        services: Option<&[Option<rsbinder::Strong<dyn INamedCallback::INamedCallback>>]>,
        names: Option<&[Option<String>]>,
    ) -> std::result::Result<bool, rsbinder::Status> {
        self.VerifyNamesWithNullableInterfaceArray(services, names)
    }

    fn RepeatParcelFileDescriptor(
        &self,
        read: &ParcelFileDescriptor,
    ) -> std::result::Result<ParcelFileDescriptor, rsbinder::Status> {
        Ok(dup_fd(read))
    }

    fn ReverseParcelFileDescriptorArray(
        &self,
        input: &[ParcelFileDescriptor],
        repeated: &mut Vec<Option<ParcelFileDescriptor>>,
    ) -> std::result::Result<Vec<ParcelFileDescriptor>, rsbinder::Status> {
        repeated.clear();
        repeated.extend(input.iter().map(dup_fd).map(Some));
        Ok(input.iter().rev().map(dup_fd).collect())
    }

    fn ThrowServiceException(&self, code: i32) -> std::result::Result<(), rsbinder::Status> {
        Err(rsbinder::Status::new_service_specific_error(code, None))
    }

    impl_repeat_nullable! {RepeatNullableIntArray, i32}
    impl_repeat_nullable! {RepeatNullableByteEnumArray, ByteEnum}
    impl_repeat_nullable! {RepeatNullableIntEnumArray, IntEnum}
    impl_repeat_nullable! {RepeatNullableLongEnumArray, LongEnum}
    impl_repeat_nullable! {RepeatNullableStringList, Option<String>}

    fn RepeatNullableString(&self, input: Option<&str>) -> std::result::Result<Option<String>, rsbinder::Status> {
        Ok(input.map(String::from))
    }

    fn RepeatNullableUtf8CppString(&self, input: Option<&str>) -> std::result::Result<Option<String>, rsbinder::Status> {
        Ok(input.map(String::from))
    }

    fn RepeatNullableParcelable(&self, input: Option<&Empty>) -> std::result::Result<Option<Empty>, rsbinder::Status> {
        Ok(input.cloned())
    }

    impl_repeat_nullable! {RepeatNullableParcelableArray, Option<Empty>}
    impl_repeat_nullable! {RepeatNullableParcelableList, Option<Empty>}

    fn TakesAnIBinder(&self, _: &SIBinder) -> std::result::Result<(), rsbinder::Status> {
        Ok(())
    }

    fn TakesANullableIBinder(&self, _: Option<&SIBinder>) -> std::result::Result<(), rsbinder::Status> {
        Ok(())
    }

    fn TakesAnIBinderList(&self, _: &[SIBinder]) -> std::result::Result<(), rsbinder::Status> {
        Ok(())
    }

    fn TakesANullableIBinderList(&self, _: Option<&[Option<SIBinder>]>) -> std::result::Result<(), rsbinder::Status> {
        Ok(())
    }

    fn ReverseNullableUtf8CppString(
        &self,
        input: Option<&[Option<String>]>,
        repeated: &mut Option<Vec<Option<String>>>,
    ) -> std::result::Result<Option<Vec<Option<String>>>, rsbinder::Status> {
        if let Some(input) = input {
            *repeated = Some(input.to_vec());
            Ok(Some(input.iter().rev().cloned().collect()))
        } else {
            // We don't touch `repeated` here, since
            // the C++ test service doesn't either
            Ok(None)
        }
    }

    fn ReverseUtf8CppStringList(
        &self,
        input: Option<&[Option<String>]>,
        repeated: &mut Option<Vec<Option<String>>>,
    ) -> std::result::Result<Option<Vec<Option<String>>>, rsbinder::Status> {
        self.ReverseNullableUtf8CppString(input, repeated)
    }

    fn GetCallback(
        &self,
        return_null: bool,
    ) -> std::result::Result<Option<rsbinder::Strong<dyn INamedCallback::INamedCallback>>, rsbinder::Status> {
        if return_null {
            Ok(None)
        } else {
            self.GetOtherTestService("ABT: always be testing").map(Some)
        }
    }

    fn FillOutStructuredParcelable(
        &self,
        parcelable: &mut StructuredParcelable::StructuredParcelable,
    ) -> std::result::Result<(), rsbinder::Status> {
        parcelable.shouldBeJerry = "Jerry".into();
        parcelable.shouldContainThreeFs = vec![parcelable.f, parcelable.f, parcelable.f];
        parcelable.shouldBeByteBar = ByteEnum::BAR;
        parcelable.shouldBeIntBar = IntEnum::BAR;
        parcelable.shouldBeLongBar = LongEnum::BAR;
        parcelable.shouldContainTwoByteFoos = vec![ByteEnum::FOO, ByteEnum::FOO];
        parcelable.shouldContainTwoIntFoos = vec![IntEnum::FOO, IntEnum::FOO];
        parcelable.shouldContainTwoLongFoos = vec![LongEnum::FOO, LongEnum::FOO];

        parcelable.const_exprs_1 = ConstantExpressionEnum::decInt32_1;
        parcelable.const_exprs_2 = ConstantExpressionEnum::decInt32_2;
        parcelable.const_exprs_3 = ConstantExpressionEnum::decInt64_1;
        parcelable.const_exprs_4 = ConstantExpressionEnum::decInt64_2;
        parcelable.const_exprs_5 = ConstantExpressionEnum::decInt64_3;
        parcelable.const_exprs_6 = ConstantExpressionEnum::decInt64_4;
        parcelable.const_exprs_7 = ConstantExpressionEnum::hexInt32_1;
        parcelable.const_exprs_8 = ConstantExpressionEnum::hexInt32_2;
        parcelable.const_exprs_9 = ConstantExpressionEnum::hexInt32_3;
        parcelable.const_exprs_10 = ConstantExpressionEnum::hexInt64_1;

        parcelable.shouldSetBit0AndBit2 = StructuredParcelable::BIT0 | StructuredParcelable::BIT2;

        parcelable.u = Some(Union::Union::Ns(vec![1, 2, 3]));
        parcelable.shouldBeConstS1 = Some(Union::Union::S(Union::S1.to_string()));
        Ok(())
    }

    fn RepeatExtendableParcelable(
        &self,
        ep: &ExtendableParcelable,
        ep2: &mut ExtendableParcelable,
    ) -> std::result::Result<(), rsbinder::Status> {
        ep2.a = ep.a;
        ep2.b = ep.b.clone();

        let my_ext = ep.ext.get_parcelable::<MyExt>()?;
        if let Some(my_ext) = my_ext {
            ep2.ext.set_parcelable(my_ext)?;
        } else {
            ep2.ext.reset();
        }

        Ok(())
    }

    fn ReverseList(&self, list: &RecursiveList) -> std::result::Result<RecursiveList, rsbinder::Status> {
        let mut reversed: Option<RecursiveList> = None;
        let mut cur: Option<&RecursiveList> = Some(list);
        while let Some(node) = cur {
            reversed = Some(RecursiveList { value: node.value, next: reversed.map(Box::new) });
            cur = node.next.as_ref().map(|n| n.as_ref());
        }
        // `list` is always not empty, so is `reversed`.
        Ok(reversed.unwrap())
    }

    fn ReverseIBinderArray(
        &self,
        input: &[SIBinder],
        repeated: &mut Vec<Option<SIBinder>>,
    ) -> std::result::Result<Vec<SIBinder>, rsbinder::Status> {
        *repeated = input.iter().cloned().map(Some).collect();
        Ok(input.iter().rev().cloned().collect())
    }

    fn ReverseNullableIBinderArray(
        &self,
        input: Option<&[Option<SIBinder>]>,
        repeated: &mut Option<Vec<Option<SIBinder>>>,
    ) -> std::result::Result<Option<Vec<Option<SIBinder>>>, rsbinder::Status> {
        let input = input.expect("input is null");
        *repeated = Some(input.to_vec());
        Ok(Some(input.iter().rev().cloned().collect()))
    }

    fn GetOldNameInterface(&self) -> std::result::Result<rsbinder::Strong<dyn IOldName::IOldName>, rsbinder::Status> {
        Ok(IOldName::BnOldName::new_binder(OldName))
    }

    fn GetNewNameInterface(&self) -> std::result::Result<rsbinder::Strong<dyn INewName::INewName>, rsbinder::Status> {
        Ok(INewName::BnNewName::new_binder(NewName))
    }

    fn GetUnionTags(&self, input: &[Union::Union]) -> std::result::Result<Vec<Union::Tag>, rsbinder::Status> {
        Ok(input
            .iter()
            .map(|u| match u {
                Union::Union::Ns(_) => Union::Tag::ns,
                Union::Union::N(_) => Union::Tag::n,
                Union::Union::M(_) => Union::Tag::m,
                Union::Union::S(_) => Union::Tag::s,
                Union::Union::Ibinder(_) => Union::Tag::ibinder,
                Union::Union::Ss(_) => Union::Tag::ss,
                Union::Union::Be(_) => Union::Tag::be,
            })
            .collect::<Vec<_>>())
    }

    fn GetCppJavaTests(&self) -> std::result::Result<Option<SIBinder>, rsbinder::Status> {
        Ok(None)
    }

    fn getBackendType(&self) -> std::result::Result<BackendType, rsbinder::Status> {
        Ok(BackendType::RUST)
    }
}


struct FooInterface;

impl Interface for FooInterface {}

impl IFooInterface::IFooInterface for FooInterface {
    fn originalApi(&self) -> std::result::Result<(), Status> {
        Ok(())
    }
    fn acceptUnionAndReturnString(&self, u: &BazUnion) -> std::result::Result<String, Status> {
        match u {
            BazUnion::IntNum(n) => Ok(n.to_string()),
            BazUnion::LongNum(n) => Ok(n.to_string()),
        }
    }
    fn returnsLengthOfFooArray(&self, foos: &[Foo]) -> std::result::Result<i32, Status> {
        Ok(foos.len() as i32)
    }
    fn ignoreParcelablesAndRepeatInt(
        &self,
        _in_foo: &Foo,
        _inout_foo: &mut Foo,
        _out_foo: &mut Foo,
        value: i32,
    ) -> std::result::Result<i32, Status> {
        Ok(value)
    }
    fn newApi(&self) -> rsbinder::status::Result<()> {
        Ok(())
    }
}

struct NestedService;

impl Interface for NestedService {}

impl INestedService::INestedService for NestedService {
    fn flipStatus(
        &self,
        p: &ParcelableWithNested::ParcelableWithNested,
    ) -> std::result::Result<INestedService::Result::Result, Status> {
        if p.status == ParcelableWithNested::Status::Status::OK {
            Ok(INestedService::Result::Result {
                status: ParcelableWithNested::Status::Status::NOT_OK,
            })
        } else {
            Ok(INestedService::Result::Result { status: ParcelableWithNested::Status::Status::OK })
        }
    }
    fn flipStatusWithCallback(
        &self,
        st: ParcelableWithNested::Status::Status,
        cb: &rsbinder::Strong<dyn INestedService::ICallback::ICallback>,
    ) -> std::result::Result<(), Status> {
        if st == ParcelableWithNested::Status::Status::OK {
            cb.done(ParcelableWithNested::Status::Status::NOT_OK)
        } else {
            cb.done(ParcelableWithNested::Status::Status::OK)
        }
    }
}

struct FixedSizeArrayService;

impl Interface for FixedSizeArrayService {}

impl IRepeatFixedSizeArray::IRepeatFixedSizeArray for FixedSizeArrayService {
    fn RepeatBytes(&self, input: &[u8; 3], repeated: &mut [u8; 3]) -> std::result::Result<[u8; 3], Status> {
        *repeated = *input;
        Ok(*input)
    }
    fn RepeatInts(&self, input: &[i32; 3], repeated: &mut [i32; 3]) -> std::result::Result<[i32; 3], Status> {
        *repeated = *input;
        Ok(*input)
    }
    fn RepeatBinders(
        &self,
        input: &[SIBinder; 3],
        repeated: &mut [Option<SIBinder>; 3],
    ) -> std::result::Result<[SIBinder; 3], Status> {
        *repeated = input.clone().map(Some);
        Ok(input.clone())
    }
    fn RepeatParcelables(
        &self,
        input: &[IntParcelable; 3],
        repeated: &mut [IntParcelable; 3],
    ) -> std::result::Result<[IntParcelable; 3], Status> {
        *repeated = *input;
        Ok(*input)
    }
    fn Repeat2dBytes(
        &self,
        input: &[[u8; 3]; 2],
        repeated: &mut [[u8; 3]; 2],
    ) -> std::result::Result<[[u8; 3]; 2], Status> {
        *repeated = *input;
        Ok(*input)
    }
    fn Repeat2dInts(
        &self,
        input: &[[i32; 3]; 2],
        repeated: &mut [[i32; 3]; 2],
    ) -> std::result::Result<[[i32; 3]; 2], Status> {
        *repeated = *input;
        Ok(*input)
    }
    fn Repeat2dBinders(
        &self,
        input: &[[SIBinder; 3]; 2],
        repeated: &mut [[Option<SIBinder>; 3]; 2],
    ) -> std::result::Result<[[SIBinder; 3]; 2], Status> {
        *repeated = input.clone().map(|nested| nested.map(Some));
        Ok(input.clone())
    }
    fn Repeat2dParcelables(
        &self,
        input: &[[IntParcelable; 3]; 2],
        repeated: &mut [[IntParcelable; 3]; 2],
    ) -> std::result::Result<[[IntParcelable; 3]; 2], Status> {
        *repeated = *input;
        Ok(*input)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // Initialize ProcessState with binder path and max threads.
    // The meaning of zero max threads is to use the default value. It is dependent on the kernel.
    ProcessState::init_default();

    let service_name = <BpTestService as ITestService::ITestService>::descriptor();
    let service = BnTestService::new_binder(TestService::default());
    rsbinder_hub::add_service(service_name, service.as_binder()).expect("Could not register service");

    let versioned_service_name = <BpFooInterface as IFooInterface::IFooInterface>::descriptor();
    let versioned_service = BnFooInterface::new_binder(FooInterface);
    rsbinder_hub::add_service(versioned_service_name, versioned_service.as_binder())
        .expect("Could not register service");

    let nested_service_name =
        <INestedService::BpNestedService as INestedService::INestedService>::descriptor();
    let nested_service =
        INestedService::BnNestedService::new_binder(NestedService);
        rsbinder_hub::add_service(nested_service_name, nested_service.as_binder())
        .expect("Could not register service");

    let fixed_size_array_service_name =
        <IRepeatFixedSizeArray::BpRepeatFixedSizeArray as IRepeatFixedSizeArray::IRepeatFixedSizeArray>::descriptor();
    let fixed_size_array_service = IRepeatFixedSizeArray::BnRepeatFixedSizeArray::new_binder(
        FixedSizeArrayService
    );
    rsbinder_hub::add_service(fixed_size_array_service_name, fixed_size_array_service.as_binder())
        .expect("Could not register service");

    Ok(ProcessState::join_thread_pool()?)
}