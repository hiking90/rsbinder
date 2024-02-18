/*
 * Copyright (C) 2022, The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
#![allow(non_snake_case,hidden_glob_reexports)]

use env_logger::Env;
pub use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/test_aidl.rs"));

/// Test Rust service for the AIDL compiler.

pub use android::aidl::fixedsizearray::FixedSizeArrayExample::{
    IRepeatFixedSizeArray, IntParcelable::IntParcelable,
};

use android::aidl::tests::nested::{
    INestedService, ParcelableWithNested,
};
use android::aidl::tests::ITestService::{
    self, BnTestService, BpTestService, Empty::Empty,
};
use android::aidl::tests::{
    extension::ExtendableParcelable::ExtendableParcelable, extension::MyExt::MyExt,
    BackendType::BackendType, ByteEnum::ByteEnum, CircularParcelable::CircularParcelable,
    ConstantExpressionEnum::ConstantExpressionEnum, ICircular, INamedCallback, INewName, IOldName,
    IntEnum::IntEnum, LongEnum::LongEnum, RecursiveList::RecursiveList, StructuredParcelable,
    Union,
};
use android::aidl::versioned::tests::{
    BazUnion::BazUnion, Foo::Foo, IFooInterface, IFooInterface::BnFooInterface,
    IFooInterface::BpFooInterface,
};
use rsbinder_tokio::Tokio;
use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

fn dup_fd(fd: &ParcelFileDescriptor) -> ParcelFileDescriptor {
    ParcelFileDescriptor::new(fd.as_ref().try_clone().unwrap())
}

struct NamedCallback(String);

impl Interface for NamedCallback {}

#[async_trait]
impl INamedCallback::INamedCallbackAsyncService for NamedCallback {
    async fn GetName(&self) -> rsbinder::status::Result<String> {
        Ok(self.0.clone())
    }
}

struct OldName;

impl Interface for OldName {}

#[async_trait]
impl IOldName::IOldNameAsyncService for OldName {
    async fn RealName(&self) -> rsbinder::status::Result<String> {
        Ok("OldName".into())
    }
}

#[derive(Debug, Default)]
struct NewName;

impl Interface for NewName {}

#[async_trait]
impl INewName::INewNameAsyncService for NewName {
    async fn RealName(&self) -> rsbinder::status::Result<String> {
        Ok("NewName".into())
    }
}

#[derive(Debug, Default)]
struct Circular;

impl Interface for Circular {}

#[async_trait]
impl ICircular::ICircularAsyncService for Circular {
    async fn GetTestService(
        &self,
    ) -> rsbinder::status::Result<Option<rsbinder::Strong<dyn ITestService::ITestService>>> {
        Ok(None)
    }
}

#[derive(Default)]
struct TestService {
    service_map: Mutex<HashMap<String, rsbinder::Strong<dyn INamedCallback::INamedCallback>>>,
}

impl Interface for TestService {
    fn dump(&self, writer: &mut dyn std::io::Write, args: &[String]) -> Result<()> {
        for arg in args {
            writeln!(writer, "{}", arg).unwrap();
        }
        Ok(())
    }
}

// Macros are expanded in the wrong order, so async_trait does not apply to
// functions defined by declarative macros.

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

macro_rules! impl_repeat {
    ($repeat_name:ident, $type:ty) => {
        fn $repeat_name<'a, 'b>(&'a self, token: $type) -> BoxFuture<'b, rsbinder::status::Result<$type>>
        where
            'a: 'b,
            Self: 'b,
        {
            Box::pin(async move { Ok(token) })
        }
    };
}

macro_rules! impl_reverse {
    ($reverse_name:ident, $type:ty) => {
        fn $reverse_name<'a, 'b, 'c, 'd>(
            &'a self,
            input: &'b [$type],
            repeated: &'c mut Vec<$type>,
        ) -> BoxFuture<'d, rsbinder::status::Result<Vec<$type>>>
        where
            'a: 'd,
            'b: 'd,
            'c: 'd,
            Self: 'd,
        {
            Box::pin(async move {
                repeated.clear();
                repeated.extend_from_slice(input);
                Ok(input.iter().rev().cloned().collect())
            })
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
        fn $repeat_nullable_name<'a, 'b, 'c>(
            &'a self,
            input: Option<&'b [$type]>,
        ) -> BoxFuture<'c, rsbinder::status::Result<Option<Vec<$type>>>>
        where
            'a: 'c,
            'b: 'c,
            Self: 'c,
        {
            Box::pin(async move { Ok(input.map(<[$type]>::to_vec)) })
        }
    };
}

#[async_trait]
impl ITestService::ITestServiceAsyncService for TestService {
    impl_repeat! {RepeatByte, i8}
    impl_reverse! {ReverseByte, u8}

    async fn UnimplementedMethod(&self, _: i32) -> rsbinder::status::Result<i32> {
        // Pretend this method hasn't been implemented
        Err(rsbinder::StatusCode::UnknownTransaction.into())
    }

    async fn TestOneway(&self) -> rsbinder::status::Result<()> {
        Err(rsbinder::StatusCode::Unknown.into())
    }

    async fn Deprecated(&self) -> rsbinder::status::Result<()> {
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

    async fn RepeatString(&self, input: &str) -> rsbinder::status::Result<String> {
        Ok(input.into())
    }

    async fn RepeatUtf8CppString(&self, input: &str) -> rsbinder::status::Result<String> {
        Ok(input.into())
    }

    async fn GetOtherTestService(
        &self,
        name: &str,
    ) -> rsbinder::status::Result<rsbinder::Strong<dyn INamedCallback::INamedCallback>> {
        let mut service_map = self.service_map.lock().unwrap();
        let other_service = service_map.entry(name.into()).or_insert_with(|| {
            let named_callback = NamedCallback(name.into());
            INamedCallback::BnNamedCallback::new_async_binder(
                named_callback,
                rt(),
            )
        });
        Ok(other_service.to_owned())
    }

    async fn SetOtherTestService(
        &self,
        name: &str,
        service: &rsbinder::Strong<dyn INamedCallback::INamedCallback>,
    ) -> rsbinder::status::Result<bool> {
        let mut service_map = self.service_map.lock().unwrap();
        if let Some(existing_service) = service_map.get(name) {
            if existing_service == service {
                return Ok(true);
            }
        }
        service_map.insert(name.into(), service.clone());
        Ok(false)
    }

    async fn VerifyName(
        &self,
        service: &rsbinder::Strong<dyn INamedCallback::INamedCallback>,
        name: &str,
    ) -> rsbinder::status::Result<bool> {
        service.clone().into_async::<Tokio>().GetName().await.map(|found_name| found_name == name)
    }

    async fn GetInterfaceArray(
        &self,
        names: &[String],
    ) -> rsbinder::status::Result<Vec<rsbinder::Strong<dyn INamedCallback::INamedCallback>>> {
        let mut res = Vec::new();
        for name in names {
            res.push(self.GetOtherTestService(name).await?);
        }
        Ok(res)
    }

    async fn VerifyNamesWithInterfaceArray(
        &self,
        services: &[rsbinder::Strong<dyn INamedCallback::INamedCallback>],
        names: &[String],
    ) -> rsbinder::status::Result<bool> {
        if services.len() == names.len() {
            for (s, n) in services.iter().zip(names) {
                if !self.VerifyName(s, n).await? {
                    return Ok(false);
                }
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn GetNullableInterfaceArray(
        &self,
        names: Option<&[Option<String>]>,
    ) -> rsbinder::status::Result<Option<Vec<Option<rsbinder::Strong<dyn INamedCallback::INamedCallback>>>>>
    {
        if let Some(names) = names {
            let mut services = vec![];
            for name in names {
                if let Some(name) = name {
                    services.push(Some(self.GetOtherTestService(name).await?));
                } else {
                    services.push(None);
                }
            }
            Ok(Some(services))
        } else {
            Ok(None)
        }
    }

    async fn VerifyNamesWithNullableInterfaceArray(
        &self,
        services: Option<&[Option<rsbinder::Strong<dyn INamedCallback::INamedCallback>>]>,
        names: Option<&[Option<String>]>,
    ) -> rsbinder::status::Result<bool> {
        if let (Some(services), Some(names)) = (services, names) {
            for (s, n) in services.iter().zip(names) {
                if let (Some(s), Some(n)) = (s, n) {
                    if !self.VerifyName(s, n).await? {
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

    async fn GetInterfaceList(
        &self,
        names: Option<&[Option<String>]>,
    ) -> rsbinder::status::Result<Option<Vec<Option<rsbinder::Strong<dyn INamedCallback::INamedCallback>>>>>
    {
        self.GetNullableInterfaceArray(names).await
    }

    async fn VerifyNamesWithInterfaceList(
        &self,
        services: Option<&[Option<rsbinder::Strong<dyn INamedCallback::INamedCallback>>]>,
        names: Option<&[Option<String>]>,
    ) -> rsbinder::status::Result<bool> {
        self.VerifyNamesWithNullableInterfaceArray(services, names).await
    }

    async fn RepeatParcelFileDescriptor(
        &self,
        read: &ParcelFileDescriptor,
    ) -> rsbinder::status::Result<ParcelFileDescriptor> {
        Ok(dup_fd(read))
    }

    async fn ReverseParcelFileDescriptorArray(
        &self,
        input: &[ParcelFileDescriptor],
        repeated: &mut Vec<Option<ParcelFileDescriptor>>,
    ) -> rsbinder::status::Result<Vec<ParcelFileDescriptor>> {
        repeated.clear();
        repeated.extend(input.iter().map(dup_fd).map(Some));
        Ok(input.iter().rev().map(dup_fd).collect())
    }

    async fn ThrowServiceException(&self, code: i32) -> rsbinder::status::Result<()> {
        Err(rsbinder::Status::new_service_specific_error(code, None))
    }

    impl_repeat_nullable! {RepeatNullableIntArray, i32}
    impl_repeat_nullable! {RepeatNullableByteEnumArray, ByteEnum}
    impl_repeat_nullable! {RepeatNullableIntEnumArray, IntEnum}
    impl_repeat_nullable! {RepeatNullableLongEnumArray, LongEnum}
    impl_repeat_nullable! {RepeatNullableStringList, Option<String>}

    async fn RepeatNullableString(&self, input: Option<&str>) -> rsbinder::status::Result<Option<String>> {
        Ok(input.map(String::from))
    }

    async fn RepeatNullableUtf8CppString(
        &self,
        input: Option<&str>,
    ) -> rsbinder::status::Result<Option<String>> {
        Ok(input.map(String::from))
    }

    async fn RepeatNullableParcelable(
        &self,
        input: Option<&Empty>,
    ) -> rsbinder::status::Result<Option<Empty>> {
        Ok(input.cloned())
    }

    impl_repeat_nullable! {RepeatNullableParcelableArray, Option<Empty>}
    impl_repeat_nullable! {RepeatNullableParcelableList, Option<Empty>}

    async fn TakesAnIBinder(&self, _: &SIBinder) -> rsbinder::status::Result<()> {
        Ok(())
    }

    async fn TakesANullableIBinder(&self, _: Option<&SIBinder>) -> rsbinder::status::Result<()> {
        Ok(())
    }

    async fn TakesAnIBinderList(&self, _: &[SIBinder]) -> rsbinder::status::Result<()> {
        Ok(())
    }

    async fn TakesANullableIBinderList(
        &self,
        _: Option<&[Option<SIBinder>]>,
    ) -> rsbinder::status::Result<()> {
        Ok(())
    }

    async fn ReverseNullableUtf8CppString(
        &self,
        input: Option<&[Option<String>]>,
        repeated: &mut Option<Vec<Option<String>>>,
    ) -> rsbinder::status::Result<Option<Vec<Option<String>>>> {
        if let Some(input) = input {
            *repeated = Some(input.to_vec());
            Ok(Some(input.iter().rev().cloned().collect()))
        } else {
            // We don't touch `repeated` here, since
            // the C++ test service doesn't either
            Ok(None)
        }
    }

    async fn ReverseUtf8CppStringList(
        &self,
        input: Option<&[Option<String>]>,
        repeated: &mut Option<Vec<Option<String>>>,
    ) -> rsbinder::status::Result<Option<Vec<Option<String>>>> {
        self.ReverseNullableUtf8CppString(input, repeated).await
    }

    async fn GetCallback(
        &self,
        return_null: bool,
    ) -> rsbinder::status::Result<Option<rsbinder::Strong<dyn INamedCallback::INamedCallback>>> {
        if return_null {
            Ok(None)
        } else {
            self.GetOtherTestService("ABT: always be testing").await.map(Some)
        }
    }

    async fn FillOutStructuredParcelable(
        &self,
        parcelable: &mut StructuredParcelable::StructuredParcelable,
    ) -> rsbinder::status::Result<()> {
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

    async fn RepeatExtendableParcelable(
        &self,
        ep: &ExtendableParcelable,
        ep2: &mut ExtendableParcelable,
    ) -> rsbinder::status::Result<()> {
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

    async fn ReverseList(&self, list: &RecursiveList) -> rsbinder::status::Result<RecursiveList> {
        let mut reversed: Option<RecursiveList> = None;
        let mut cur: Option<&RecursiveList> = Some(list);
        while let Some(node) = cur {
            reversed = Some(RecursiveList { value: node.value, next: reversed.map(Box::new) });
            cur = node.next.as_ref().map(|n| n.as_ref());
        }
        // `list` is always not empty, so is `reversed`.
        Ok(reversed.unwrap())
    }

    async fn ReverseIBinderArray(
        &self,
        input: &[SIBinder],
        repeated: &mut Vec<Option<SIBinder>>,
    ) -> rsbinder::status::Result<Vec<SIBinder>> {
        *repeated = input.iter().cloned().map(Some).collect();
        Ok(input.iter().rev().cloned().collect())
    }

    async fn ReverseNullableIBinderArray(
        &self,
        input: Option<&[Option<SIBinder>]>,
        repeated: &mut Option<Vec<Option<SIBinder>>>,
    ) -> rsbinder::status::Result<Option<Vec<Option<SIBinder>>>> {
        let input = input.expect("input is null");
        *repeated = Some(input.to_vec());
        Ok(Some(input.iter().rev().cloned().collect()))
    }

    async fn GetOldNameInterface(&self) -> rsbinder::status::Result<rsbinder::Strong<dyn IOldName::IOldName>> {
        Ok(IOldName::BnOldName::new_async_binder(OldName, rt()))
    }

    async fn GetNewNameInterface(&self) -> rsbinder::status::Result<rsbinder::Strong<dyn INewName::INewName>> {
        Ok(INewName::BnNewName::new_async_binder(NewName, rt()))
    }

    async fn GetUnionTags(&self, input: &[Union::Union]) -> rsbinder::status::Result<Vec<Union::Tag>> {
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

    async fn GetCppJavaTests(&self) -> rsbinder::status::Result<Option<SIBinder>> {
        Ok(None)
    }

    async fn getBackendType(&self) -> rsbinder::status::Result<BackendType> {
        Ok(BackendType::RUST)
    }

    async fn GetCircular(
        &self,
        _: &mut CircularParcelable,
    ) -> rsbinder::status::Result<rsbinder::Strong<dyn ICircular::ICircular>> {
        Ok(ICircular::BnCircular::new_async_binder(Circular, rt()))
    }
}

struct FooInterface;

impl Interface for FooInterface {}

#[async_trait]
impl IFooInterface::IFooInterfaceAsyncService for FooInterface {
    async fn originalApi(&self) -> rsbinder::status::Result<()> {
        Ok(())
    }
    async fn acceptUnionAndReturnString(&self, u: &BazUnion) -> rsbinder::status::Result<String> {
        match u {
            BazUnion::IntNum(n) => Ok(n.to_string()),
            BazUnion::LongNum(n) => Ok(n.to_string()),
        }
    }
    async fn returnsLengthOfFooArray(&self, foos: &[Foo]) -> rsbinder::status::Result<i32> {
        Ok(foos.len() as i32)
    }
    async fn ignoreParcelablesAndRepeatInt(
        &self,
        _in_foo: &Foo,
        _inout_foo: &mut Foo,
        _out_foo: &mut Foo,
        value: i32,
    ) -> rsbinder::status::Result<i32> {
        Ok(value)
    }
    async fn newApi(&self) -> rsbinder::status::Result<()> {
        Ok(())
    }
}

struct NestedService;

impl Interface for NestedService {}

#[async_trait]
impl INestedService::INestedServiceAsyncService for NestedService {
    async fn flipStatus(
        &self,
        p: &ParcelableWithNested::ParcelableWithNested,
    ) -> rsbinder::status::Result<INestedService::Result::Result> {
        if p.status == ParcelableWithNested::Status::Status::OK {
            Ok(INestedService::Result::Result {
                status: ParcelableWithNested::Status::Status::NOT_OK,
            })
        } else {
            Ok(INestedService::Result::Result { status: ParcelableWithNested::Status::Status::OK })
        }
    }
    async fn flipStatusWithCallback(
        &self,
        st: ParcelableWithNested::Status::Status,
        cb: &rsbinder::Strong<dyn INestedService::ICallback::ICallback>,
    ) -> rsbinder::status::Result<()> {
        if st == ParcelableWithNested::Status::Status::OK {
            cb.done(ParcelableWithNested::Status::Status::NOT_OK)
        } else {
            cb.done(ParcelableWithNested::Status::Status::OK)
        }
    }
}

struct FixedSizeArrayService;

impl Interface for FixedSizeArrayService {}

#[async_trait]
impl IRepeatFixedSizeArray::IRepeatFixedSizeArrayAsyncService for FixedSizeArrayService {
    async fn RepeatBytes(
        &self,
        input: &[u8; 3],
        repeated: &mut [u8; 3],
    ) -> rsbinder::status::Result<[u8; 3]> {
        *repeated = *input;
        Ok(*input)
    }
    async fn RepeatInts(
        &self,
        input: &[i32; 3],
        repeated: &mut [i32; 3],
    ) -> rsbinder::status::Result<[i32; 3]> {
        *repeated = *input;
        Ok(*input)
    }
    async fn RepeatBinders(
        &self,
        input: &[SIBinder; 3],
        repeated: &mut [Option<SIBinder>; 3],
    ) -> rsbinder::status::Result<[SIBinder; 3]> {
        *repeated = input.clone().map(Some);
        Ok(input.clone())
    }
    async fn RepeatParcelables(
        &self,
        input: &[IntParcelable; 3],
        repeated: &mut [IntParcelable; 3],
    ) -> rsbinder::status::Result<[IntParcelable; 3]> {
        *repeated = *input;
        Ok(*input)
    }
    async fn Repeat2dBytes(
        &self,
        input: &[[u8; 3]; 2],
        repeated: &mut [[u8; 3]; 2],
    ) -> rsbinder::status::Result<[[u8; 3]; 2]> {
        *repeated = *input;
        Ok(*input)
    }
    async fn Repeat2dInts(
        &self,
        input: &[[i32; 3]; 2],
        repeated: &mut [[i32; 3]; 2],
    ) -> rsbinder::status::Result<[[i32; 3]; 2]> {
        *repeated = *input;
        Ok(*input)
    }
    async fn Repeat2dBinders(
        &self,
        input: &[[SIBinder; 3]; 2],
        repeated: &mut [[Option<SIBinder>; 3]; 2],
    ) -> rsbinder::status::Result<[[SIBinder; 3]; 2]> {
        *repeated = input.clone().map(|nested| nested.map(Some));
        Ok(input.clone())
    }
    async fn Repeat2dParcelables(
        &self,
        input: &[[IntParcelable; 3]; 2],
        repeated: &mut [[IntParcelable; 3]; 2],
    ) -> rsbinder::status::Result<[[IntParcelable; 3]; 2]> {
        *repeated = *input;
        Ok(*input)
    }
}

fn rt() -> rsbinder_tokio::TokioRuntime<tokio::runtime::Runtime> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rsbinder_tokio::TokioRuntime(rt)
}

fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    ProcessState::init_default();
    ProcessState::start_thread_pool();

    let service_name = <BpTestService as ITestService::ITestService>::descriptor();
    let service =
        BnTestService::new_async_binder(TestService::default(), rt());
    rsbinder_hub::add_service(service_name, service.as_binder()).expect("Could not register service");

    let versioned_service_name = <BpFooInterface as IFooInterface::IFooInterface>::descriptor();
    let versioned_service =
        BnFooInterface::new_async_binder(FooInterface, rt());
    rsbinder_hub::add_service(versioned_service_name, versioned_service.as_binder())
        .expect("Could not register service");

    let nested_service_name =
        <INestedService::BpNestedService as INestedService::INestedService>::descriptor();
    let nested_service = INestedService::BnNestedService::new_async_binder(
        NestedService,
        rt(),
    );
    rsbinder_hub::add_service(nested_service_name, nested_service.as_binder())
        .expect("Could not register service");

    let fixed_size_array_service_name =
        <IRepeatFixedSizeArray::BpRepeatFixedSizeArray as IRepeatFixedSizeArray::IRepeatFixedSizeArray>::descriptor();
    let fixed_size_array_service = IRepeatFixedSizeArray::BnRepeatFixedSizeArray::new_async_binder(
        FixedSizeArrayService,
        rt(),
    );
    rsbinder_hub::add_service(fixed_size_array_service_name, fixed_size_array_service.as_binder())
    .expect("Could not register service");

    ProcessState::join_thread_pool().expect("Failed to join thread pool");
}
