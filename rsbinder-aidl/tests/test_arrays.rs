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

#[test]
fn test_nullability() -> Result<(), Box<dyn Error>> {
    aidl_generator(
        r##"
package android.aidl.fixedsizearray;
interface ITestService {
    // Test that arrays work as parameters and return types.
    boolean[] ReverseBoolean(in boolean[] input, out boolean[] repeated);

    @nullable int[] RepeatNullableIntArray(in @nullable int[] input);
    void FillOutStructuredParcelable(inout StructuredParcelable parcel);
}
        "##,
        r##"
pub mod ITestService {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    pub trait ITestService: rsbinder::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "android.aidl.fixedsizearray.ITestService" }
        fn r#ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>) -> rsbinder::status::Result<Vec<bool>>;
        fn r#RepeatNullableIntArray(&self, _arg_input: Option<&[i32]>) -> rsbinder::status::Result<Option<Vec<i32>>>;
        fn r#FillOutStructuredParcelable(&self, _arg_parcel: &mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::status::Result<()>;
        fn getDefaultImpl() -> Option<ITestServiceDefaultRef> where Self: Sized {
            DEFAULT_IMPL.get().cloned()
        }
        fn setDefaultImpl(d: ITestServiceDefaultRef) -> ITestServiceDefaultRef where Self: Sized {
            DEFAULT_IMPL.get_or_init(|| d).clone()
        }
    }
    pub trait ITestServiceAsync<P>: rsbinder::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "android.aidl.fixedsizearray.ITestService" }
        fn r#ReverseBoolean<'a>(&'a self, _arg_input: &'a [bool], _arg_repeated: &'a mut Vec<bool>) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<Vec<bool>>>;
        fn r#RepeatNullableIntArray<'a>(&'a self, _arg_input: Option<&'a [i32]>) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<Option<Vec<i32>>>>;
        fn r#FillOutStructuredParcelable<'a>(&'a self, _arg_parcel: &'a mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<()>>;
    }
    #[::async_trait::async_trait]
    pub trait ITestServiceAsyncService: rsbinder::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "android.aidl.fixedsizearray.ITestService" }
        async fn r#ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>) -> rsbinder::status::Result<Vec<bool>>;
        async fn r#RepeatNullableIntArray(&self, _arg_input: Option<&[i32]>) -> rsbinder::status::Result<Option<Vec<i32>>>;
        async fn r#FillOutStructuredParcelable(&self, _arg_parcel: &mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::status::Result<()>;
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
                fn r#ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>) -> rsbinder::status::Result<Vec<bool>> {
                    self._rt.block_on(self._inner.r#ReverseBoolean(_arg_input, _arg_repeated))
                }
                fn r#RepeatNullableIntArray(&self, _arg_input: Option<&[i32]>) -> rsbinder::status::Result<Option<Vec<i32>>> {
                    self._rt.block_on(self._inner.r#RepeatNullableIntArray(_arg_input))
                }
                fn r#FillOutStructuredParcelable(&self, _arg_parcel: &mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::status::Result<()> {
                    self._rt.block_on(self._inner.r#FillOutStructuredParcelable(_arg_parcel))
                }
            }
            let wrapped = Wrapper { _inner: inner, _rt: rt };
            let binder = rsbinder::native::Binder::new_with_stability(BnTestService(Box::new(wrapped)), rsbinder::Stability::default());
            rsbinder::Strong::new(Box::new(binder))
        }
    }
    pub trait ITestServiceDefault: Send + Sync {
        fn r#ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>) -> rsbinder::status::Result<Vec<bool>> {
            Err(rsbinder::StatusCode::UnknownTransaction.into())
        }
        fn r#RepeatNullableIntArray(&self, _arg_input: Option<&[i32]>) -> rsbinder::status::Result<Option<Vec<i32>>> {
            Err(rsbinder::StatusCode::UnknownTransaction.into())
        }
        fn r#FillOutStructuredParcelable(&self, _arg_parcel: &mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::status::Result<()> {
            Err(rsbinder::StatusCode::UnknownTransaction.into())
        }
    }
    pub(crate) mod transactions {
        pub(crate) const r#ReverseBoolean: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;
        pub(crate) const r#RepeatNullableIntArray: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 1;
        pub(crate) const r#FillOutStructuredParcelable: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 2;
    }
    pub type ITestServiceDefaultRef = std::sync::Arc<dyn ITestServiceDefault>;
    static DEFAULT_IMPL: std::sync::OnceLock<ITestServiceDefaultRef> = std::sync::OnceLock::new();
    rsbinder::declare_binder_interface! {
        ITestService["android.aidl.fixedsizearray.ITestService"] {
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
        fn build_parcel_ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.binder.as_proxy().unwrap().prepare_transact(true)?;
            data.write(_arg_input)?;
            data.write_slice_size(Some(_arg_repeated))?;
            Ok(data)
        }
        fn read_response_ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>, _aidl_reply: rsbinder::Result<Option<rsbinder::Parcel>>) -> rsbinder::status::Result<Vec<bool>> {
            if let Err(rsbinder::StatusCode::UnknownTransaction) = _aidl_reply {
                if let Some(_aidl_default_impl) = <Self as ITestService>::getDefaultImpl() {
                  return _aidl_default_impl.r#ReverseBoolean(_arg_input, _arg_repeated);
                }
            }
            let mut _aidl_reply = _aidl_reply?.ok_or(rsbinder::StatusCode::UnexpectedNull)?;
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            if !_status.is_ok() { return Err(_status); }
            let _aidl_return: Vec<bool> = _aidl_reply.read()?;
            _aidl_reply.read_onto(_arg_repeated)?;
            Ok(_aidl_return)
        }
        fn build_parcel_RepeatNullableIntArray(&self, _arg_input: Option<&[i32]>) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.binder.as_proxy().unwrap().prepare_transact(true)?;
            data.write(&_arg_input)?;
            Ok(data)
        }
        fn read_response_RepeatNullableIntArray(&self, _arg_input: Option<&[i32]>, _aidl_reply: rsbinder::Result<Option<rsbinder::Parcel>>) -> rsbinder::status::Result<Option<Vec<i32>>> {
            if let Err(rsbinder::StatusCode::UnknownTransaction) = _aidl_reply {
                if let Some(_aidl_default_impl) = <Self as ITestService>::getDefaultImpl() {
                  return _aidl_default_impl.r#RepeatNullableIntArray(_arg_input);
                }
            }
            let mut _aidl_reply = _aidl_reply?.ok_or(rsbinder::StatusCode::UnexpectedNull)?;
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            if !_status.is_ok() { return Err(_status); }
            let _aidl_return: Option<Vec<i32>> = _aidl_reply.read()?;
            Ok(_aidl_return)
        }
        fn build_parcel_FillOutStructuredParcelable(&self, _arg_parcel: &mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.binder.as_proxy().unwrap().prepare_transact(true)?;
            data.write(_arg_parcel)?;
            Ok(data)
        }
        fn read_response_FillOutStructuredParcelable(&self, _arg_parcel: &mut rsbinder::Strong<dyn StructuredParcelable>, _aidl_reply: rsbinder::Result<Option<rsbinder::Parcel>>) -> rsbinder::status::Result<()> {
            if let Err(rsbinder::StatusCode::UnknownTransaction) = _aidl_reply {
                if let Some(_aidl_default_impl) = <Self as ITestService>::getDefaultImpl() {
                  return _aidl_default_impl.r#FillOutStructuredParcelable(_arg_parcel);
                }
            }
            let mut _aidl_reply = _aidl_reply?.ok_or(rsbinder::StatusCode::UnexpectedNull)?;
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            if !_status.is_ok() { return Err(_status); }
            _aidl_reply.read_onto(_arg_parcel)?;
            Ok(())
        }
    }
    impl ITestService for BpTestService {
        fn r#ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>) -> rsbinder::status::Result<Vec<bool>> {
            let _aidl_data = self.build_parcel_ReverseBoolean(_arg_input, _arg_repeated)?;
            let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::r#ReverseBoolean, &_aidl_data, rsbinder::FLAG_CLEAR_BUF);
            self.read_response_ReverseBoolean(_arg_input, _arg_repeated, _aidl_reply)
        }
        fn r#RepeatNullableIntArray(&self, _arg_input: Option<&[i32]>) -> rsbinder::status::Result<Option<Vec<i32>>> {
            let _aidl_data = self.build_parcel_RepeatNullableIntArray(_arg_input)?;
            let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::r#RepeatNullableIntArray, &_aidl_data, rsbinder::FLAG_CLEAR_BUF);
            self.read_response_RepeatNullableIntArray(_arg_input, _aidl_reply)
        }
        fn r#FillOutStructuredParcelable(&self, _arg_parcel: &mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::status::Result<()> {
            let _aidl_data = self.build_parcel_FillOutStructuredParcelable(_arg_parcel)?;
            let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::r#FillOutStructuredParcelable, &_aidl_data, rsbinder::FLAG_CLEAR_BUF);
            self.read_response_FillOutStructuredParcelable(_arg_parcel, _aidl_reply)
        }
    }
    impl<P: rsbinder::BinderAsyncPool> ITestServiceAsync<P> for BpTestService {
        fn r#ReverseBoolean<'a>(&'a self, _arg_input: &'a [bool], _arg_repeated: &'a mut Vec<bool>) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<Vec<bool>>> {
            let _aidl_data = match self.build_parcel_ReverseBoolean(_arg_input, _arg_repeated) {
                Ok(_aidl_data) => _aidl_data,
                Err(err) => return Box::pin(std::future::ready(Err(err.into()))),
            };
            let binder = self.binder.clone();
            P::spawn(
                move || binder.as_proxy().unwrap().submit_transact(transactions::r#ReverseBoolean, &_aidl_data, rsbinder::FLAG_CLEAR_BUF | rsbinder::FLAG_PRIVATE_LOCAL),
                move |_aidl_reply| async move {
                    self.read_response_ReverseBoolean(_arg_input, _arg_repeated, _aidl_reply)
                }
            )
        }
        fn r#RepeatNullableIntArray<'a>(&'a self, _arg_input: Option<&'a [i32]>) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<Option<Vec<i32>>>> {
            let _aidl_data = match self.build_parcel_RepeatNullableIntArray(_arg_input) {
                Ok(_aidl_data) => _aidl_data,
                Err(err) => return Box::pin(std::future::ready(Err(err.into()))),
            };
            let binder = self.binder.clone();
            P::spawn(
                move || binder.as_proxy().unwrap().submit_transact(transactions::r#RepeatNullableIntArray, &_aidl_data, rsbinder::FLAG_CLEAR_BUF | rsbinder::FLAG_PRIVATE_LOCAL),
                move |_aidl_reply| async move {
                    self.read_response_RepeatNullableIntArray(_arg_input, _aidl_reply)
                }
            )
        }
        fn r#FillOutStructuredParcelable<'a>(&'a self, _arg_parcel: &'a mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<()>> {
            let _aidl_data = match self.build_parcel_FillOutStructuredParcelable(_arg_parcel) {
                Ok(_aidl_data) => _aidl_data,
                Err(err) => return Box::pin(std::future::ready(Err(err.into()))),
            };
            let binder = self.binder.clone();
            P::spawn(
                move || binder.as_proxy().unwrap().submit_transact(transactions::r#FillOutStructuredParcelable, &_aidl_data, rsbinder::FLAG_CLEAR_BUF | rsbinder::FLAG_PRIVATE_LOCAL),
                move |_aidl_reply| async move {
                    self.read_response_FillOutStructuredParcelable(_arg_parcel, _aidl_reply)
                }
            )
        }
    }
    impl<P: rsbinder::BinderAsyncPool> ITestServiceAsync<P> for rsbinder::Binder<BnTestService>
    {
        fn r#ReverseBoolean<'a>(&'a self, _arg_input: &'a [bool], _arg_repeated: &'a mut Vec<bool>) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<Vec<bool>>> {
            self.0.as_async().r#ReverseBoolean(_arg_input, _arg_repeated)
        }
        fn r#RepeatNullableIntArray<'a>(&'a self, _arg_input: Option<&'a [i32]>) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<Option<Vec<i32>>>> {
            self.0.as_async().r#RepeatNullableIntArray(_arg_input)
        }
        fn r#FillOutStructuredParcelable<'a>(&'a self, _arg_parcel: &'a mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<()>> {
            self.0.as_async().r#FillOutStructuredParcelable(_arg_parcel)
        }
    }
    impl ITestService for rsbinder::Binder<BnTestService> {
        fn r#ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>) -> rsbinder::status::Result<Vec<bool>> {
            self.0.as_sync().r#ReverseBoolean(_arg_input, _arg_repeated)
        }
        fn r#RepeatNullableIntArray(&self, _arg_input: Option<&[i32]>) -> rsbinder::status::Result<Option<Vec<i32>>> {
            self.0.as_sync().r#RepeatNullableIntArray(_arg_input)
        }
        fn r#FillOutStructuredParcelable(&self, _arg_parcel: &mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::status::Result<()> {
            self.0.as_sync().r#FillOutStructuredParcelable(_arg_parcel)
        }
    }
    fn on_transact(
        _service: &dyn ITestService, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
        match _code {
            transactions::r#ReverseBoolean => {
                let _arg_input: Vec<bool> = _reader.read()?;
                let mut _arg_repeated: Vec<bool> = Default::default();
                _reader.resize_out_vec(&mut _arg_repeated)?;
                let _aidl_return = _service.r#ReverseBoolean(&_arg_input, &mut _arg_repeated);
                match &_aidl_return {
                    Ok(_aidl_return) => {
                        _reply.write(&rsbinder::Status::from(rsbinder::StatusCode::Ok))?;
                        _reply.write(_aidl_return)?;
                        _reply.write(&_arg_repeated)?;
                    }
                    Err(_aidl_status) => {
                        _reply.write(_aidl_status)?;
                    }
                }
                Ok(())
            }
            transactions::r#RepeatNullableIntArray => {
                let _arg_input: Option<Vec<i32>> = _reader.read()?;
                let _aidl_return = _service.r#RepeatNullableIntArray(_arg_input.as_deref());
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
            transactions::r#FillOutStructuredParcelable => {
                let mut _arg_parcel: rsbinder::Strong<dyn StructuredParcelable> = _reader.read()?;
                let _aidl_return = _service.r#FillOutStructuredParcelable(&mut _arg_parcel);
                match &_aidl_return {
                    Ok(_aidl_return) => {
                        _reply.write(&rsbinder::Status::from(rsbinder::StatusCode::Ok))?;
                        _reply.write(&_arg_parcel)?;
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
    )
}

#[test]
fn test_fixed_size_array() -> Result<(), Box<dyn Error>> {
    aidl_generator(
        r##"
package android.aidl.fixedsizearray;
parcelable FixedSizeArrayExample {
    // to see if NxM array works
    int[2][3] int2x3 = {{1, 2, 3}, {4, 5, 6}};
    @nullable @utf8InCpp String[2][2] stringNullableMatrix = {
        {"hello", "world"}, {"Ciao", "mondo"}};
    @nullable ByteEnum[2][2] byteEnumNullableMatrix;
    @nullable IEmptyInterface[2][2] interfaceNullableMatrix;

    @SuppressWarnings(value={"out-array"})
    interface IRepeatFixedSizeArray {
        IntParcelable[2][3] Repeat2dParcelables(
            in IntParcelable[2][3] input, out IntParcelable[2][3] repeated);
    }
    enum ByteEnum { A }

    @JavaDerive(equals=true)
    @RustDerive(Clone=true, Copy=true, PartialEq=true)
    parcelable IntParcelable {
        int value;
    }

    interface IEmptyInterface {}
}
        "##,
        r##"
pub mod FixedSizeArrayExample {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    pub struct FixedSizeArrayExample {
        pub r#int2x3: [[i32; 3]; 2],
        pub r#stringNullableMatrix: Option<[[Option<String>; 2]; 2]>,
        pub r#byteEnumNullableMatrix: Option<[[ByteEnum::ByteEnum; 2]; 2]>,
        pub r#interfaceNullableMatrix: Option<[[Option<rsbinder::Strong<dyn IEmptyInterface::IEmptyInterface>>; 2]; 2]>,
    }
    impl Default for FixedSizeArrayExample {
        fn default() -> Self {
            Self {
                r#int2x3: [[1,2,3,],[4,5,6,],],
                r#stringNullableMatrix: Some([[Some("hello".into()),Some("world".into()),],[Some("Ciao".into()),Some("mondo".into()),],]),
                r#byteEnumNullableMatrix: Default::default(),
                r#interfaceNullableMatrix: Default::default(),
            }
        }
    }
    impl rsbinder::Parcelable for FixedSizeArrayExample {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                _sub_parcel.write(&self.r#int2x3)?;
                _sub_parcel.write(&self.r#stringNullableMatrix)?;
                _sub_parcel.write(&self.r#byteEnumNullableMatrix)?;
                _sub_parcel.write(&self.r#interfaceNullableMatrix)?;
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                self.r#int2x3 = _sub_parcel.read()?;
                self.r#stringNullableMatrix = _sub_parcel.read()?;
                self.r#byteEnumNullableMatrix = _sub_parcel.read()?;
                self.r#interfaceNullableMatrix = _sub_parcel.read()?;
                Ok(())
            })
        }
    }
    rsbinder::impl_serialize_for_parcelable!(FixedSizeArrayExample);
    rsbinder::impl_deserialize_for_parcelable!(FixedSizeArrayExample);
    impl rsbinder::ParcelableMetadata for FixedSizeArrayExample {
        fn descriptor() -> &'static str { "android.aidl.fixedsizearray.FixedSizeArrayExample" }
    }
    pub mod IRepeatFixedSizeArray {
        #![allow(non_upper_case_globals, non_snake_case, dead_code)]
        pub trait IRepeatFixedSizeArray: rsbinder::Interface + Send {
            fn descriptor() -> &'static str where Self: Sized { "android.aidl.fixedsizearray.FixedSizeArrayExample.IRepeatFixedSizeArray" }
            fn r#Repeat2dParcelables(&self, _arg_input: &[[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]>;
            fn getDefaultImpl() -> Option<IRepeatFixedSizeArrayDefaultRef> where Self: Sized {
                DEFAULT_IMPL.get().cloned()
            }
            fn setDefaultImpl(d: IRepeatFixedSizeArrayDefaultRef) -> IRepeatFixedSizeArrayDefaultRef where Self: Sized {
                DEFAULT_IMPL.get_or_init(|| d).clone()
            }
        }
        pub trait IRepeatFixedSizeArrayAsync<P>: rsbinder::Interface + Send {
            fn descriptor() -> &'static str where Self: Sized { "android.aidl.fixedsizearray.FixedSizeArrayExample.IRepeatFixedSizeArray" }
            fn r#Repeat2dParcelables<'a>(&'a self, _arg_input: &'a [[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &'a mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]>>;
        }
        #[::async_trait::async_trait]
        pub trait IRepeatFixedSizeArrayAsyncService: rsbinder::Interface + Send {
            fn descriptor() -> &'static str where Self: Sized { "android.aidl.fixedsizearray.FixedSizeArrayExample.IRepeatFixedSizeArray" }
            async fn r#Repeat2dParcelables(&self, _arg_input: &[[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]>;
        }
        impl BnRepeatFixedSizeArray
        {
            pub fn new_async_binder<T, R>(inner: T, rt: R) -> rsbinder::Strong<dyn IRepeatFixedSizeArray>
            where
                T: IRepeatFixedSizeArrayAsyncService + Sync + Send + 'static,
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
                impl<T, R> BnRepeatFixedSizeArrayAdapter for Wrapper<T, R>
                where
                    T: IRepeatFixedSizeArrayAsyncService + Sync + Send + 'static,
                    R: rsbinder::BinderAsyncRuntime + Send + Sync + 'static,
                {
                    fn as_sync(&self) -> &dyn IRepeatFixedSizeArray {
                        self
                    }
                    fn as_async(&self) -> &dyn IRepeatFixedSizeArrayAsyncService {
                        &self._inner
                    }
                }
                impl<T, R> IRepeatFixedSizeArray for Wrapper<T, R>
                where
                    T: IRepeatFixedSizeArrayAsyncService + Sync + Send + 'static,
                    R: rsbinder::BinderAsyncRuntime + Send + Sync + 'static,
                {
                    fn r#Repeat2dParcelables(&self, _arg_input: &[[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]> {
                        self._rt.block_on(self._inner.r#Repeat2dParcelables(_arg_input, _arg_repeated))
                    }
                }
                let wrapped = Wrapper { _inner: inner, _rt: rt };
                let binder = rsbinder::native::Binder::new_with_stability(BnRepeatFixedSizeArray(Box::new(wrapped)), rsbinder::Stability::default());
                rsbinder::Strong::new(Box::new(binder))
            }
        }
        pub trait IRepeatFixedSizeArrayDefault: Send + Sync {
            fn r#Repeat2dParcelables(&self, _arg_input: &[[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]> {
                Err(rsbinder::StatusCode::UnknownTransaction.into())
            }
        }
        pub(crate) mod transactions {
            pub(crate) const r#Repeat2dParcelables: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;
        }
        pub type IRepeatFixedSizeArrayDefaultRef = std::sync::Arc<dyn IRepeatFixedSizeArrayDefault>;
        static DEFAULT_IMPL: std::sync::OnceLock<IRepeatFixedSizeArrayDefaultRef> = std::sync::OnceLock::new();
        rsbinder::declare_binder_interface! {
            IRepeatFixedSizeArray["android.aidl.fixedsizearray.FixedSizeArrayExample.IRepeatFixedSizeArray"] {
                native: {
                    BnRepeatFixedSizeArray(on_transact),
                    adapter: BnRepeatFixedSizeArrayAdapter,
                    r#async: IRepeatFixedSizeArrayAsyncService,
                },
                proxy: BpRepeatFixedSizeArray,
                r#async: IRepeatFixedSizeArrayAsync,
            }
        }
        impl BpRepeatFixedSizeArray {
            fn build_parcel_Repeat2dParcelables(&self, _arg_input: &[[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::Result<rsbinder::Parcel> {
                let mut data = self.binder.as_proxy().unwrap().prepare_transact(true)?;
                data.write(_arg_input)?;
                Ok(data)
            }
            fn read_response_Repeat2dParcelables(&self, _arg_input: &[[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &mut [[super::IntParcelable::IntParcelable; 3]; 2], _aidl_reply: rsbinder::Result<Option<rsbinder::Parcel>>) -> rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]> {
                if let Err(rsbinder::StatusCode::UnknownTransaction) = _aidl_reply {
                    if let Some(_aidl_default_impl) = <Self as IRepeatFixedSizeArray>::getDefaultImpl() {
                      return _aidl_default_impl.r#Repeat2dParcelables(_arg_input, _arg_repeated);
                    }
                }
                let mut _aidl_reply = _aidl_reply?.ok_or(rsbinder::StatusCode::UnexpectedNull)?;
                let _status = _aidl_reply.read::<rsbinder::Status>()?;
                if !_status.is_ok() { return Err(_status); }
                let _aidl_return: [[super::IntParcelable::IntParcelable; 3]; 2] = _aidl_reply.read()?;
                _aidl_reply.read_onto(_arg_repeated)?;
                Ok(_aidl_return)
            }
        }
        impl IRepeatFixedSizeArray for BpRepeatFixedSizeArray {
            fn r#Repeat2dParcelables(&self, _arg_input: &[[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]> {
                let _aidl_data = self.build_parcel_Repeat2dParcelables(_arg_input, _arg_repeated)?;
                let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::r#Repeat2dParcelables, &_aidl_data, rsbinder::FLAG_CLEAR_BUF);
                self.read_response_Repeat2dParcelables(_arg_input, _arg_repeated, _aidl_reply)
            }
        }
        impl<P: rsbinder::BinderAsyncPool> IRepeatFixedSizeArrayAsync<P> for BpRepeatFixedSizeArray {
            fn r#Repeat2dParcelables<'a>(&'a self, _arg_input: &'a [[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &'a mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]>> {
                let _aidl_data = match self.build_parcel_Repeat2dParcelables(_arg_input, _arg_repeated) {
                    Ok(_aidl_data) => _aidl_data,
                    Err(err) => return Box::pin(std::future::ready(Err(err.into()))),
                };
                let binder = self.binder.clone();
                P::spawn(
                    move || binder.as_proxy().unwrap().submit_transact(transactions::r#Repeat2dParcelables, &_aidl_data, rsbinder::FLAG_CLEAR_BUF | rsbinder::FLAG_PRIVATE_LOCAL),
                    move |_aidl_reply| async move {
                        self.read_response_Repeat2dParcelables(_arg_input, _arg_repeated, _aidl_reply)
                    }
                )
            }
        }
        impl<P: rsbinder::BinderAsyncPool> IRepeatFixedSizeArrayAsync<P> for rsbinder::Binder<BnRepeatFixedSizeArray>
        {
            fn r#Repeat2dParcelables<'a>(&'a self, _arg_input: &'a [[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &'a mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]>> {
                self.0.as_async().r#Repeat2dParcelables(_arg_input, _arg_repeated)
            }
        }
        impl IRepeatFixedSizeArray for rsbinder::Binder<BnRepeatFixedSizeArray> {
            fn r#Repeat2dParcelables(&self, _arg_input: &[[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]> {
                self.0.as_sync().r#Repeat2dParcelables(_arg_input, _arg_repeated)
            }
        }
        fn on_transact(
            _service: &dyn IRepeatFixedSizeArray, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match _code {
                transactions::r#Repeat2dParcelables => {
                    let _arg_input: [[super::IntParcelable::IntParcelable; 3]; 2] = _reader.read()?;
                    let mut _arg_repeated: [[super::IntParcelable::IntParcelable; 3]; 2] = Default::default();
                    let _aidl_return = _service.r#Repeat2dParcelables(&_arg_input, &mut _arg_repeated);
                    match &_aidl_return {
                        Ok(_aidl_return) => {
                            _reply.write(&rsbinder::Status::from(rsbinder::StatusCode::Ok))?;
                            _reply.write(_aidl_return)?;
                            _reply.write(&_arg_repeated)?;
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
    pub mod ByteEnum {
        #![allow(non_upper_case_globals, non_snake_case)]
        rsbinder::declare_binder_enum! {
            r#ByteEnum : [i8; 1] {
                r#A = 0,
            }
        }
    }
    pub mod IntParcelable {
        #![allow(non_upper_case_globals, non_snake_case, dead_code)]
        #[derive(Debug)]
        #[derive(Clone,Copy,PartialEq)]
        pub struct IntParcelable {
            pub r#value: i32,
        }
        impl Default for IntParcelable {
            fn default() -> Self {
                Self {
                    r#value: Default::default(),
                }
            }
        }
        impl rsbinder::Parcelable for IntParcelable {
            fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
                _parcel.sized_write(|_sub_parcel| {
                    _sub_parcel.write(&self.r#value)?;
                    Ok(())
                })
            }
            fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
                _parcel.sized_read(|_sub_parcel| {
                    self.r#value = _sub_parcel.read()?;
                    Ok(())
                })
            }
        }
        rsbinder::impl_serialize_for_parcelable!(IntParcelable);
        rsbinder::impl_deserialize_for_parcelable!(IntParcelable);
        impl rsbinder::ParcelableMetadata for IntParcelable {
            fn descriptor() -> &'static str { "android.aidl.fixedsizearray.FixedSizeArrayExample.IntParcelable" }
        }
    }
    pub mod IEmptyInterface {
        #![allow(non_upper_case_globals, non_snake_case, dead_code)]
        pub trait IEmptyInterface: rsbinder::Interface + Send {
            fn descriptor() -> &'static str where Self: Sized { "android.aidl.fixedsizearray.FixedSizeArrayExample.IEmptyInterface" }
            fn getDefaultImpl() -> Option<IEmptyInterfaceDefaultRef> where Self: Sized {
                DEFAULT_IMPL.get().cloned()
            }
            fn setDefaultImpl(d: IEmptyInterfaceDefaultRef) -> IEmptyInterfaceDefaultRef where Self: Sized {
                DEFAULT_IMPL.get_or_init(|| d).clone()
            }
        }
        pub trait IEmptyInterfaceAsync<P>: rsbinder::Interface + Send {
            fn descriptor() -> &'static str where Self: Sized { "android.aidl.fixedsizearray.FixedSizeArrayExample.IEmptyInterface" }
        }
        #[::async_trait::async_trait]
        pub trait IEmptyInterfaceAsyncService: rsbinder::Interface + Send {
            fn descriptor() -> &'static str where Self: Sized { "android.aidl.fixedsizearray.FixedSizeArrayExample.IEmptyInterface" }
        }
        impl BnEmptyInterface
        {
            pub fn new_async_binder<T, R>(inner: T, rt: R) -> rsbinder::Strong<dyn IEmptyInterface>
            where
                T: IEmptyInterfaceAsyncService + Sync + Send + 'static,
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
                impl<T, R> BnEmptyInterfaceAdapter for Wrapper<T, R>
                where
                    T: IEmptyInterfaceAsyncService + Sync + Send + 'static,
                    R: rsbinder::BinderAsyncRuntime + Send + Sync + 'static,
                {
                    fn as_sync(&self) -> &dyn IEmptyInterface {
                        self
                    }
                    fn as_async(&self) -> &dyn IEmptyInterfaceAsyncService {
                        &self._inner
                    }
                }
                impl<T, R> IEmptyInterface for Wrapper<T, R>
                where
                    T: IEmptyInterfaceAsyncService + Sync + Send + 'static,
                    R: rsbinder::BinderAsyncRuntime + Send + Sync + 'static,
                {
                }
                let wrapped = Wrapper { _inner: inner, _rt: rt };
                let binder = rsbinder::native::Binder::new_with_stability(BnEmptyInterface(Box::new(wrapped)), rsbinder::Stability::default());
                rsbinder::Strong::new(Box::new(binder))
            }
        }
        pub trait IEmptyInterfaceDefault: Send + Sync {
        }
        pub(crate) mod transactions {
        }
        pub type IEmptyInterfaceDefaultRef = std::sync::Arc<dyn IEmptyInterfaceDefault>;
        static DEFAULT_IMPL: std::sync::OnceLock<IEmptyInterfaceDefaultRef> = std::sync::OnceLock::new();
        rsbinder::declare_binder_interface! {
            IEmptyInterface["android.aidl.fixedsizearray.FixedSizeArrayExample.IEmptyInterface"] {
                native: {
                    BnEmptyInterface(on_transact),
                    adapter: BnEmptyInterfaceAdapter,
                    r#async: IEmptyInterfaceAsyncService,
                },
                proxy: BpEmptyInterface,
                r#async: IEmptyInterfaceAsync,
            }
        }
        impl BpEmptyInterface {
        }
        impl IEmptyInterface for BpEmptyInterface {
        }
        impl<P: rsbinder::BinderAsyncPool> IEmptyInterfaceAsync<P> for BpEmptyInterface {
        }
        impl<P: rsbinder::BinderAsyncPool> IEmptyInterfaceAsync<P> for rsbinder::Binder<BnEmptyInterface>
        {
        }
        impl IEmptyInterface for rsbinder::Binder<BnEmptyInterface> {
        }
        fn on_transact(
            _service: &dyn IEmptyInterface, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match _code {
                _ => Err(rsbinder::StatusCode::UnknownTransaction),
            }
        }
    }
}
        "##,
    )
}