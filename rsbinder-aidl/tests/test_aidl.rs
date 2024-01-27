// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::error::Error;
use similar::{ChangeTag, TextDiff};

fn aidl_generator(input: &str, expect: &str) -> Result<(), Box<dyn Error>> {
    let document = rsbinder_aidl::parse_document(input)?;
    let res = rsbinder_aidl::gen_document(&document)?;
    let diff = TextDiff::from_lines(res.1.trim(), expect.trim());
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "- ",
            ChangeTag::Insert => "+ ",
            ChangeTag::Equal => "  ",
        };
        print!("{}{}", sign, change);
    }
    assert_eq!(res.1.trim(), expect.trim());
    Ok(())
}

#[test]
fn test_nullability() -> Result<(), Box<dyn Error>> {
    aidl_generator(r##"
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
        fn ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>) -> rsbinder::status::Result<Vec<bool>>;
        fn RepeatNullableIntArray(&self, _arg_input: Option<&[i32]>) -> rsbinder::status::Result<Option<Vec<i32>>>;
        fn FillOutStructuredParcelable(&self, _arg_parcel: &mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::status::Result<()>;
        fn getDefaultImpl() -> ITestServiceDefaultRef where Self: Sized {
            DEFAULT_IMPL.lock().unwrap().clone()
        }
        fn setDefaultImpl(d: ITestServiceDefaultRef) -> ITestServiceDefaultRef where Self: Sized {
            std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
        }
    }
    pub trait ITestServiceDefault: Send + Sync {
        fn ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>) -> rsbinder::status::Result<Vec<bool>> {
            Err(rsbinder::StatusCode::UnknownTransaction.into())
        }
        fn RepeatNullableIntArray(&self, _arg_input: Option<&[i32]>) -> rsbinder::status::Result<Option<Vec<i32>>> {
            Err(rsbinder::StatusCode::UnknownTransaction.into())
        }
        fn FillOutStructuredParcelable(&self, _arg_parcel: &mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::status::Result<()> {
            Err(rsbinder::StatusCode::UnknownTransaction.into())
        }
    }
    pub(crate) mod transactions {
        pub(crate) const ReverseBoolean: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;
        pub(crate) const RepeatNullableIntArray: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 1;
        pub(crate) const FillOutStructuredParcelable: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 2;
    }
    pub type ITestServiceDefaultRef = Option<std::sync::Arc<dyn ITestServiceDefault>>;
    use lazy_static::lazy_static;
    lazy_static! {
        static ref DEFAULT_IMPL: std::sync::Mutex<ITestServiceDefaultRef> = std::sync::Mutex::new(None);
    }
    rsbinder::declare_binder_interface! {
        ITestService["android.aidl.fixedsizearray.ITestService"] {
            native: BnTestService(on_transact),
            proxy: BpTestService,
        }
    }
    impl BpTestService {
        fn build_parcel_ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.binder.as_proxy().unwrap().prepare_transact(true)?;
            data.write(_arg_input)?;
            Ok(data)
        }
        fn read_response_ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>, _aidl_reply: rsbinder::Result<Option<rsbinder::Parcel>>) -> rsbinder::status::Result<Vec<bool>> {
            if let Err(rsbinder::StatusCode::UnknownTransaction) = _aidl_reply {
                if let Some(_aidl_default_impl) = <Self as ITestService>::getDefaultImpl() {
                  return _aidl_default_impl.ReverseBoolean(_arg_input, _arg_repeated);
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
                  return _aidl_default_impl.RepeatNullableIntArray(_arg_input);
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
                  return _aidl_default_impl.FillOutStructuredParcelable(_arg_parcel);
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
        fn ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>) -> rsbinder::status::Result<Vec<bool>> {
            let _aidl_data = self.build_parcel_ReverseBoolean(_arg_input, _arg_repeated)?;
            let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::ReverseBoolean, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR);
            self.read_response_ReverseBoolean(_arg_input, _arg_repeated, _aidl_reply)
        }
        fn RepeatNullableIntArray(&self, _arg_input: Option<&[i32]>) -> rsbinder::status::Result<Option<Vec<i32>>> {
            let _aidl_data = self.build_parcel_RepeatNullableIntArray(_arg_input)?;
            let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::RepeatNullableIntArray, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR);
            self.read_response_RepeatNullableIntArray(_arg_input, _aidl_reply)
        }
        fn FillOutStructuredParcelable(&self, _arg_parcel: &mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::status::Result<()> {
            let _aidl_data = self.build_parcel_FillOutStructuredParcelable(_arg_parcel)?;
            let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::FillOutStructuredParcelable, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR);
            self.read_response_FillOutStructuredParcelable(_arg_parcel, _aidl_reply)
        }
    }
    impl ITestService for rsbinder::Binder<BnTestService> {
        fn ReverseBoolean(&self, _arg_input: &[bool], _arg_repeated: &mut Vec<bool>) -> rsbinder::status::Result<Vec<bool>> {
            self.0.ReverseBoolean(_arg_input, _arg_repeated)
        }
        fn RepeatNullableIntArray(&self, _arg_input: Option<&[i32]>) -> rsbinder::status::Result<Option<Vec<i32>>> {
            self.0.RepeatNullableIntArray(_arg_input)
        }
        fn FillOutStructuredParcelable(&self, _arg_parcel: &mut rsbinder::Strong<dyn StructuredParcelable>) -> rsbinder::status::Result<()> {
            self.0.FillOutStructuredParcelable(_arg_parcel)
        }
    }
    fn on_transact(
        _service: &dyn ITestService, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel, _descriptor: &str) -> rsbinder::Result<()> {
        match _code {
            transactions::ReverseBoolean => {
                let _arg_input: Vec<bool> = _reader.read()?;
                let mut _arg_repeated: Vec<bool> = Default::default();
                let _aidl_return = _service.ReverseBoolean(&_arg_input, &mut _arg_repeated);
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
            transactions::RepeatNullableIntArray => {
                let _arg_input: Option<Vec<i32>> = _reader.read()?;
                let _aidl_return = _service.RepeatNullableIntArray(_arg_input.as_deref());
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
            transactions::FillOutStructuredParcelable => {
                let mut _arg_parcel: rsbinder::Strong<dyn StructuredParcelable> = _reader.read()?;
                let _aidl_return = _service.FillOutStructuredParcelable(&mut _arg_parcel);
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
        "##)
}

#[test]
fn test_fixed_size_array() -> Result<(), Box<dyn Error>> {
    aidl_generator(r##"
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
        pub int2x3: [[i32; 3]; 2],
        pub stringNullableMatrix: Option<[[Option<String>; 2]; 2]>,
        pub byteEnumNullableMatrix: Option<[[ByteEnum::ByteEnum; 2]; 2]>,
        pub interfaceNullableMatrix: Option<[[Option<rsbinder::Strong<dyn IEmptyInterface::IEmptyInterface>>; 2]; 2]>,
    }
    impl Default for FixedSizeArrayExample {
        fn default() -> Self {
            Self {
                int2x3: [[1,2,3,],[4,5,6,],],
                stringNullableMatrix: Some([[Some("hello".into()),Some("world".into()),],[Some("Ciao".into()),Some("mondo".into()),],]),
                byteEnumNullableMatrix: Default::default(),
                interfaceNullableMatrix: Default::default(),
            }
        }
    }
    impl rsbinder::Parcelable for FixedSizeArrayExample {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                _sub_parcel.write(&self.int2x3)?;
                _sub_parcel.write(&self.stringNullableMatrix)?;
                _sub_parcel.write(&self.byteEnumNullableMatrix)?;
                _sub_parcel.write(&self.interfaceNullableMatrix)?;
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                self.int2x3 = _sub_parcel.read()?;
                self.stringNullableMatrix = _sub_parcel.read()?;
                self.byteEnumNullableMatrix = _sub_parcel.read()?;
                self.interfaceNullableMatrix = _sub_parcel.read()?;
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
            fn Repeat2dParcelables(&self, _arg_input: &[[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]>;
            fn getDefaultImpl() -> IRepeatFixedSizeArrayDefaultRef where Self: Sized {
                DEFAULT_IMPL.lock().unwrap().clone()
            }
            fn setDefaultImpl(d: IRepeatFixedSizeArrayDefaultRef) -> IRepeatFixedSizeArrayDefaultRef where Self: Sized {
                std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
            }
        }
        pub trait IRepeatFixedSizeArrayDefault: Send + Sync {
            fn Repeat2dParcelables(&self, _arg_input: &[[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]> {
                Err(rsbinder::StatusCode::UnknownTransaction.into())
            }
        }
        pub(crate) mod transactions {
            pub(crate) const Repeat2dParcelables: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;
        }
        pub type IRepeatFixedSizeArrayDefaultRef = Option<std::sync::Arc<dyn IRepeatFixedSizeArrayDefault>>;
        use lazy_static::lazy_static;
        lazy_static! {
            static ref DEFAULT_IMPL: std::sync::Mutex<IRepeatFixedSizeArrayDefaultRef> = std::sync::Mutex::new(None);
        }
        rsbinder::declare_binder_interface! {
            IRepeatFixedSizeArray["android.aidl.fixedsizearray.FixedSizeArrayExample.IRepeatFixedSizeArray"] {
                native: BnRepeatFixedSizeArray(on_transact),
                proxy: BpRepeatFixedSizeArray,
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
                      return _aidl_default_impl.Repeat2dParcelables(_arg_input, _arg_repeated);
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
            fn Repeat2dParcelables(&self, _arg_input: &[[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]> {
                let _aidl_data = self.build_parcel_Repeat2dParcelables(_arg_input, _arg_repeated)?;
                let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::Repeat2dParcelables, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR);
                self.read_response_Repeat2dParcelables(_arg_input, _arg_repeated, _aidl_reply)
            }
        }
        impl IRepeatFixedSizeArray for rsbinder::Binder<BnRepeatFixedSizeArray> {
            fn Repeat2dParcelables(&self, _arg_input: &[[super::IntParcelable::IntParcelable; 3]; 2], _arg_repeated: &mut [[super::IntParcelable::IntParcelable; 3]; 2]) -> rsbinder::status::Result<[[super::IntParcelable::IntParcelable; 3]; 2]> {
                self.0.Repeat2dParcelables(_arg_input, _arg_repeated)
            }
        }
        fn on_transact(
            _service: &dyn IRepeatFixedSizeArray, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel, _descriptor: &str) -> rsbinder::Result<()> {
            match _code {
                transactions::Repeat2dParcelables => {
                    let _arg_input: [[super::IntParcelable::IntParcelable; 3]; 2] = _reader.read()?;
                    let mut _arg_repeated: [[super::IntParcelable::IntParcelable; 3]; 2] = Default::default();
                    let _aidl_return = _service.Repeat2dParcelables(&_arg_input, &mut _arg_repeated);
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
            ByteEnum : [i8; 1] {
                A = 0,
            }
        }
    }
    pub mod IntParcelable {
        #![allow(non_upper_case_globals, non_snake_case, dead_code)]
        #[derive(Debug)]
        #[derive(Clone,Copy,PartialEq)]
        pub struct IntParcelable {
            pub value: i32,
        }
        impl Default for IntParcelable {
            fn default() -> Self {
                Self {
                    value: Default::default(),
                }
            }
        }
        impl rsbinder::Parcelable for IntParcelable {
            fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
                _parcel.sized_write(|_sub_parcel| {
                    _sub_parcel.write(&self.value)?;
                    Ok(())
                })
            }
            fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
                _parcel.sized_read(|_sub_parcel| {
                    self.value = _sub_parcel.read()?;
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
            fn getDefaultImpl() -> IEmptyInterfaceDefaultRef where Self: Sized {
                DEFAULT_IMPL.lock().unwrap().clone()
            }
            fn setDefaultImpl(d: IEmptyInterfaceDefaultRef) -> IEmptyInterfaceDefaultRef where Self: Sized {
                std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
            }
        }
        pub trait IEmptyInterfaceDefault: Send + Sync {
        }
        pub(crate) mod transactions {
        }
        pub type IEmptyInterfaceDefaultRef = Option<std::sync::Arc<dyn IEmptyInterfaceDefault>>;
        use lazy_static::lazy_static;
        lazy_static! {
            static ref DEFAULT_IMPL: std::sync::Mutex<IEmptyInterfaceDefaultRef> = std::sync::Mutex::new(None);
        }
        rsbinder::declare_binder_interface! {
            IEmptyInterface["android.aidl.fixedsizearray.FixedSizeArrayExample.IEmptyInterface"] {
                native: BnEmptyInterface(on_transact),
                proxy: BpEmptyInterface,
            }
        }
        impl BpEmptyInterface {
        }
        impl IEmptyInterface for BpEmptyInterface {
        }
        impl IEmptyInterface for rsbinder::Binder<BnEmptyInterface> {
        }
        fn on_transact(
            _service: &dyn IEmptyInterface, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel, _descriptor: &str) -> rsbinder::Result<()> {
            match _code {
                _ => Err(rsbinder::StatusCode::UnknownTransaction),
            }
        }
    }
}

        "##)
}

#[test]
fn test_array_of_interfaces_check() -> Result<(), Box<dyn Error>> {
    aidl_generator(r##"
parcelable ArrayOfInterfaces {
    interface IEmptyInterface {}

    interface IMyInterface {
        @nullable String[] methodWithInterfaces(IEmptyInterface iface,
                @nullable IEmptyInterface nullable_iface,
                in IEmptyInterface[] iface_array_in, out IEmptyInterface[] iface_array_out,
                inout IEmptyInterface[] iface_array_inout,
                in @nullable IEmptyInterface[] nullable_iface_array_in,
                out @nullable IEmptyInterface[] nullable_iface_array_out,
                inout @nullable IEmptyInterface[] nullable_iface_array_inout);
    }
}
        "##,
        r##"
pub mod ArrayOfInterfaces {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    pub struct ArrayOfInterfaces {
    }
    impl Default for ArrayOfInterfaces {
        fn default() -> Self {
            Self {
            }
        }
    }
    impl rsbinder::Parcelable for ArrayOfInterfaces {
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
    rsbinder::impl_serialize_for_parcelable!(ArrayOfInterfaces);
    rsbinder::impl_deserialize_for_parcelable!(ArrayOfInterfaces);
    impl rsbinder::ParcelableMetadata for ArrayOfInterfaces {
        fn descriptor() -> &'static str { "ArrayOfInterfaces" }
    }
    pub mod IEmptyInterface {
        #![allow(non_upper_case_globals, non_snake_case, dead_code)]
        pub trait IEmptyInterface: rsbinder::Interface + Send {
            fn descriptor() -> &'static str where Self: Sized { "ArrayOfInterfaces.IEmptyInterface" }
            fn getDefaultImpl() -> IEmptyInterfaceDefaultRef where Self: Sized {
                DEFAULT_IMPL.lock().unwrap().clone()
            }
            fn setDefaultImpl(d: IEmptyInterfaceDefaultRef) -> IEmptyInterfaceDefaultRef where Self: Sized {
                std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
            }
        }
        pub trait IEmptyInterfaceDefault: Send + Sync {
        }
        pub(crate) mod transactions {
        }
        pub type IEmptyInterfaceDefaultRef = Option<std::sync::Arc<dyn IEmptyInterfaceDefault>>;
        use lazy_static::lazy_static;
        lazy_static! {
            static ref DEFAULT_IMPL: std::sync::Mutex<IEmptyInterfaceDefaultRef> = std::sync::Mutex::new(None);
        }
        rsbinder::declare_binder_interface! {
            IEmptyInterface["ArrayOfInterfaces.IEmptyInterface"] {
                native: BnEmptyInterface(on_transact),
                proxy: BpEmptyInterface,
            }
        }
        impl BpEmptyInterface {
        }
        impl IEmptyInterface for BpEmptyInterface {
        }
        impl IEmptyInterface for rsbinder::Binder<BnEmptyInterface> {
        }
        fn on_transact(
            _service: &dyn IEmptyInterface, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel, _descriptor: &str) -> rsbinder::Result<()> {
            match _code {
                _ => Err(rsbinder::StatusCode::UnknownTransaction),
            }
        }
    }
    pub mod IMyInterface {
        #![allow(non_upper_case_globals, non_snake_case, dead_code)]
        pub trait IMyInterface: rsbinder::Interface + Send {
            fn descriptor() -> &'static str where Self: Sized { "ArrayOfInterfaces.IMyInterface" }
            fn methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::status::Result<Option<Vec<Option<String>>>>;
            fn getDefaultImpl() -> IMyInterfaceDefaultRef where Self: Sized {
                DEFAULT_IMPL.lock().unwrap().clone()
            }
            fn setDefaultImpl(d: IMyInterfaceDefaultRef) -> IMyInterfaceDefaultRef where Self: Sized {
                std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
            }
        }
        pub trait IMyInterfaceDefault: Send + Sync {
            fn methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::status::Result<Option<Vec<Option<String>>>> {
                Err(rsbinder::StatusCode::UnknownTransaction.into())
            }
        }
        pub(crate) mod transactions {
            pub(crate) const methodWithInterfaces: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;
        }
        pub type IMyInterfaceDefaultRef = Option<std::sync::Arc<dyn IMyInterfaceDefault>>;
        use lazy_static::lazy_static;
        lazy_static! {
            static ref DEFAULT_IMPL: std::sync::Mutex<IMyInterfaceDefaultRef> = std::sync::Mutex::new(None);
        }
        rsbinder::declare_binder_interface! {
            IMyInterface["ArrayOfInterfaces.IMyInterface"] {
                native: BnMyInterface(on_transact),
                proxy: BpMyInterface,
            }
        }
        impl BpMyInterface {
            fn build_parcel_methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::Result<rsbinder::Parcel> {
                let mut data = self.binder.as_proxy().unwrap().prepare_transact(true)?;
                data.write(_arg_iface)?;
                data.write(&_arg_nullable_iface)?;
                data.write(_arg_iface_array_in)?;
                data.write(_arg_iface_array_inout)?;
                data.write(&_arg_nullable_iface_array_in)?;
                data.write(_arg_nullable_iface_array_inout)?;
                Ok(data)
            }
            fn read_response_methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _aidl_reply: rsbinder::Result<Option<rsbinder::Parcel>>) -> rsbinder::status::Result<Option<Vec<Option<String>>>> {
                if let Err(rsbinder::StatusCode::UnknownTransaction) = _aidl_reply {
                    if let Some(_aidl_default_impl) = <Self as IMyInterface>::getDefaultImpl() {
                      return _aidl_default_impl.methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_array_in, _arg_iface_array_out, _arg_iface_array_inout, _arg_nullable_iface_array_in, _arg_nullable_iface_array_out, _arg_nullable_iface_array_inout);
                    }
                }
                let mut _aidl_reply = _aidl_reply?.ok_or(rsbinder::StatusCode::UnexpectedNull)?;
                let _status = _aidl_reply.read::<rsbinder::Status>()?;
                if !_status.is_ok() { return Err(_status); }
                let _aidl_return: Option<Vec<Option<String>>> = _aidl_reply.read()?;
                _aidl_reply.read_onto(_arg_iface_array_out)?;
                _aidl_reply.read_onto(_arg_iface_array_inout)?;
                _aidl_reply.read_onto(_arg_nullable_iface_array_out)?;
                _aidl_reply.read_onto(_arg_nullable_iface_array_inout)?;
                Ok(_aidl_return)
            }
        }
        impl IMyInterface for BpMyInterface {
            fn methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::status::Result<Option<Vec<Option<String>>>> {
                let _aidl_data = self.build_parcel_methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_array_in, _arg_iface_array_out, _arg_iface_array_inout, _arg_nullable_iface_array_in, _arg_nullable_iface_array_out, _arg_nullable_iface_array_inout)?;
                let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::methodWithInterfaces, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR);
                self.read_response_methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_array_in, _arg_iface_array_out, _arg_iface_array_inout, _arg_nullable_iface_array_in, _arg_nullable_iface_array_out, _arg_nullable_iface_array_inout, _aidl_reply)
            }
        }
        impl IMyInterface for rsbinder::Binder<BnMyInterface> {
            fn methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::status::Result<Option<Vec<Option<String>>>> {
                self.0.methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_array_in, _arg_iface_array_out, _arg_iface_array_inout, _arg_nullable_iface_array_in, _arg_nullable_iface_array_out, _arg_nullable_iface_array_inout)
            }
        }
        fn on_transact(
            _service: &dyn IMyInterface, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel, _descriptor: &str) -> rsbinder::Result<()> {
            match _code {
                transactions::methodWithInterfaces => {
                    let _arg_iface: rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface> = _reader.read()?;
                    let _arg_nullable_iface: Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>> = _reader.read()?;
                    let _arg_iface_array_in: Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>> = _reader.read()?;
                    let mut _arg_iface_array_out: Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>> = Default::default();
                    let mut _arg_iface_array_inout: Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>> = _reader.read()?;
                    let _arg_nullable_iface_array_in: Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>> = _reader.read()?;
                    let mut _arg_nullable_iface_array_out: Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>> = Default::default();
                    let mut _arg_nullable_iface_array_inout: Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>> = _reader.read()?;
                    let _aidl_return = _service.methodWithInterfaces(&_arg_iface, _arg_nullable_iface.as_ref(), &_arg_iface_array_in, &mut _arg_iface_array_out, &mut _arg_iface_array_inout, _arg_nullable_iface_array_in.as_deref(), &mut _arg_nullable_iface_array_out, &mut _arg_nullable_iface_array_inout);
                    match &_aidl_return {
                        Ok(_aidl_return) => {
                            _reply.write(&rsbinder::Status::from(rsbinder::StatusCode::Ok))?;
                            _reply.write(_aidl_return)?;
                            _reply.write(&_arg_iface_array_out)?;
                            _reply.write(&_arg_iface_array_inout)?;
                            _reply.write(&_arg_nullable_iface_array_out)?;
                            _reply.write(&_arg_nullable_iface_array_inout)?;
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
}
        "##)
}

#[test]
fn test_parcelable_const_name() -> Result<(), Box<dyn Error>> {
    aidl_generator(r##"
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
    pub const DUMP_FLAG_PRIORITY_CRITICAL: i32 = 1;
    pub const DUMP_FLAG_PRIORITY_HIGH: i32 = 2;
    pub const DUMP_FLAG_PRIORITY_NORMAL: i32 = 4;
    pub const DUMP_FLAG_PRIORITY_DEFAULT: i32 = 8;
    pub const DUMP_FLAG_PRIORITY_ALL: i32 = 15;
    pub const DUMP_FLAG_PROTO: i32 = 16;
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
        "#)?;
    Ok(())
}

#[test]
fn test_parcelable() -> Result<(), Box<dyn Error>> {
    aidl_generator(r##"
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
        "##, r#"
pub mod ConnectionInfo {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    pub struct ConnectionInfo {
        pub ipAddress: String,
        pub port: i32,
    }
    impl Default for ConnectionInfo {
        fn default() -> Self {
            Self {
                ipAddress: Default::default(),
                port: Default::default(),
            }
        }
    }
    impl rsbinder::Parcelable for ConnectionInfo {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                _sub_parcel.write(&self.ipAddress)?;
                _sub_parcel.write(&self.port)?;
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                self.ipAddress = _sub_parcel.read()?;
                self.port = _sub_parcel.read()?;
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
        "#)?;
    Ok(())
}

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
fn test_unions() -> Result<(), Box<dyn Error>> {
    aidl_generator(&(r#"
        package android.aidl.tests;

        @Backing(type="byte")
        enum ByteEnum {
            // Comment about FOO.
            FOO = 1,
            BAR = 2,
            BAZ,
        }

        "#.to_owned() + UNION),
        r#"
pub mod ByteEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        ByteEnum : [i8; 3] {
            FOO = 1,
            BAR = 2,
            BAZ = 3,
        }
    }
}
pub mod Union {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    #[derive(Clone,PartialEq)]
    pub enum Union {
        Ns(Vec<i32>),
        N(i32),
        M(i32),
        S(String),
        Ibinder(Option<rsbinder::SIBinder>),
        Ss(Vec<String>),
        Be(super::ByteEnum::ByteEnum),
    }
    pub const S1: &str = "a string constant in union";
    impl Default for Union {
        fn default() -> Self {
            Self::Ns(Default::default())
        }
    }
    impl rsbinder::Parcelable for Union {
        fn write_to_parcel(&self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match self {
                Self::Ns(v) => {
                    parcel.write(&0i32)?;
                    parcel.write(v)
                }
                Self::N(v) => {
                    parcel.write(&1i32)?;
                    parcel.write(v)
                }
                Self::M(v) => {
                    parcel.write(&2i32)?;
                    parcel.write(v)
                }
                Self::S(v) => {
                    parcel.write(&3i32)?;
                    parcel.write(v)
                }
                Self::Ibinder(v) => {
                    parcel.write(&4i32)?;
                    parcel.write(v)
                }
                Self::Ss(v) => {
                    parcel.write(&5i32)?;
                    parcel.write(v)
                }
                Self::Be(v) => {
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
                    *self = Self::Ns(value);
                    Ok(())
                }
                1 => {
                    let value: i32 = parcel.read()?;
                    *self = Self::N(value);
                    Ok(())
                }
                2 => {
                    let value: i32 = parcel.read()?;
                    *self = Self::M(value);
                    Ok(())
                }
                3 => {
                    let value: String = parcel.read()?;
                    *self = Self::S(value);
                    Ok(())
                }
                4 => {
                    let value: Option<rsbinder::SIBinder> = parcel.read()?;
                    *self = Self::Ibinder(value);
                    Ok(())
                }
                5 => {
                    let value: Vec<String> = parcel.read()?;
                    *self = Self::Ss(value);
                    Ok(())
                }
                6 => {
                    let value: super::ByteEnum::ByteEnum = parcel.read()?;
                    *self = Self::Be(value);
                    Ok(())
                }
                _ => Err(rsbinder::StatusCode::BadValue),
            }
        }
    }
    rsbinder::impl_serialize_for_parcelable!(Union);
    rsbinder::impl_deserialize_for_parcelable!(Union);
    impl rsbinder::ParcelableMetadata for Union {
        fn descriptor() -> &'static str { "android.aidl.tests.Union" }
    }
    rsbinder::declare_binder_enum! {
        Tag : [i32; 7] {
            ns = 0,
            n = 1,
            m = 2,
            s = 3,
            ibinder = 4,
            ss = 5,
            be = 6,
        }
    }
}
        "#)?;

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
const LONG_ENUM: &str =r#"
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
fn test_enums() -> Result<(), Box<dyn Error>> {
    aidl_generator(BYTE_ENUM,
        r##"
pub mod ByteEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        ByteEnum : [i8; 3] {
            FOO = 1,
            BAR = 2,
            BAZ = 3,
        }
    }
}
        "##)?;

    aidl_generator(r##"
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
        BackendType : [i8; 4] {
            CPP = 0,
            JAVA = 1,
            NDK = 2,
            RUST = 3,
        }
    }
}
        "##)?;

    aidl_generator(CONSTANT_EXPRESSION_ENUM,
        r##"
pub mod ConstantExpressionEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        ConstantExpressionEnum : [i32; 10] {
            decInt32_1 = 1,
            decInt32_2 = 1,
            decInt64_1 = 1,
            decInt64_2 = 1,
            decInt64_3 = 1,
            decInt64_4 = 1,
            hexInt32_1 = 1,
            hexInt32_2 = 1,
            hexInt32_3 = 1,
            hexInt64_1 = 1,
        }
    }
}
        "##)?;

    aidl_generator(INT_ENUM,
        r##"
pub mod IntEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        IntEnum : [i32; 4] {
            FOO = 1000,
            BAR = 2000,
            BAZ = 2001,
            QUX = 2002,
        }
    }
}
        "##)?;

    aidl_generator(LONG_ENUM,
        r##"
pub mod LongEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        LongEnum : [i64; 3] {
            FOO = 100000000000,
            BAR = 200000000000,
            BAZ = 200000000001,
        }
    }
}
        "##)?;

    aidl_generator(&(BYTE_ENUM.to_owned() + r##"
interface ITestService {
    ByteEnum RepeatByteEnum(ByteEnum token);
}
        "##),
        r##"
pub mod ByteEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        ByteEnum : [i8; 3] {
            FOO = 1,
            BAR = 2,
            BAZ = 3,
        }
    }
}
pub mod ITestService {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    pub trait ITestService: rsbinder::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "ITestService" }
        fn RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::status::Result<super::ByteEnum::ByteEnum>;
        fn getDefaultImpl() -> ITestServiceDefaultRef where Self: Sized {
            DEFAULT_IMPL.lock().unwrap().clone()
        }
        fn setDefaultImpl(d: ITestServiceDefaultRef) -> ITestServiceDefaultRef where Self: Sized {
            std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
        }
    }
    pub trait ITestServiceDefault: Send + Sync {
        fn RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::status::Result<super::ByteEnum::ByteEnum> {
            Err(rsbinder::StatusCode::UnknownTransaction.into())
        }
    }
    pub(crate) mod transactions {
        pub(crate) const RepeatByteEnum: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;
    }
    pub type ITestServiceDefaultRef = Option<std::sync::Arc<dyn ITestServiceDefault>>;
    use lazy_static::lazy_static;
    lazy_static! {
        static ref DEFAULT_IMPL: std::sync::Mutex<ITestServiceDefaultRef> = std::sync::Mutex::new(None);
    }
    rsbinder::declare_binder_interface! {
        ITestService["ITestService"] {
            native: BnTestService(on_transact),
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
                  return _aidl_default_impl.RepeatByteEnum(_arg_token);
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
        fn RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::status::Result<super::ByteEnum::ByteEnum> {
            let _aidl_data = self.build_parcel_RepeatByteEnum(_arg_token)?;
            let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::RepeatByteEnum, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR);
            self.read_response_RepeatByteEnum(_arg_token, _aidl_reply)
        }
    }
    impl ITestService for rsbinder::Binder<BnTestService> {
        fn RepeatByteEnum(&self, _arg_token: super::ByteEnum::ByteEnum) -> rsbinder::status::Result<super::ByteEnum::ByteEnum> {
            self.0.RepeatByteEnum(_arg_token)
        }
    }
    fn on_transact(
        _service: &dyn ITestService, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel, _descriptor: &str) -> rsbinder::Result<()> {
        match _code {
            transactions::RepeatByteEnum => {
                let _arg_token: super::ByteEnum::ByteEnum = _reader.read()?;
                let _aidl_return = _service.RepeatByteEnum(_arg_token);
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

        "##)?;

    Ok(())
}

#[test]
fn test_byte_parcelable() -> Result<(), Box<dyn Error>> {
    aidl_generator(&(r#"
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
        "#.to_owned() + CONSTANT_EXPRESSION_ENUM + BYTE_ENUM + INT_ENUM + LONG_ENUM + UNION),
        r#"
pub mod StructuredParcelable {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    pub const BIT0: i32 = 1;
    pub const BIT1: i32 = 2;
    pub const BIT2: i32 = 4;
    #[derive(Debug)]
    pub struct StructuredParcelable {
        pub shouldContainThreeFs: Vec<i32>,
        pub f: i32,
        pub shouldBeJerry: String,
        pub shouldBeByteBar: super::ByteEnum::ByteEnum,
        pub shouldBeIntBar: super::IntEnum::IntEnum,
        pub shouldBeLongBar: super::LongEnum::LongEnum,
        pub shouldContainTwoByteFoos: Vec<super::ByteEnum::ByteEnum>,
        pub shouldContainTwoIntFoos: Vec<super::IntEnum::IntEnum>,
        pub shouldContainTwoLongFoos: Vec<super::LongEnum::LongEnum>,
        pub stringDefaultsToFoo: String,
        pub byteDefaultsToFour: i8,
        pub intDefaultsToFive: i32,
        pub longDefaultsToNegativeSeven: i64,
        pub booleanDefaultsToTrue: bool,
        pub charDefaultsToC: u16,
        pub floatDefaultsToPi: f32,
        pub doubleWithDefault: f64,
        pub arrayDefaultsTo123: Vec<i32>,
        pub arrayDefaultsToEmpty: Vec<i32>,
        pub boolDefault: bool,
        pub byteDefault: i8,
        pub intDefault: i32,
        pub longDefault: i64,
        pub floatDefault: f32,
        pub doubleDefault: f64,
        pub checkDoubleFromFloat: f64,
        pub checkStringArray1: Vec<String>,
        pub checkStringArray2: Vec<String>,
        pub int32_min: i32,
        pub int32_max: i32,
        pub int64_max: i64,
        pub hexInt32_neg_1: i32,
        pub ibinder: Option<rsbinder::SIBinder>,
        pub empty: Empty::Empty,
        pub int8_1: Vec<u8>,
        pub int32_1: Vec<i32>,
        pub int64_1: Vec<i64>,
        pub hexInt32_pos_1: i32,
        pub hexInt64_pos_1: i32,
        pub const_exprs_1: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub const_exprs_2: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub const_exprs_3: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub const_exprs_4: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub const_exprs_5: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub const_exprs_6: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub const_exprs_7: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub const_exprs_8: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub const_exprs_9: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub const_exprs_10: super::ConstantExpressionEnum::ConstantExpressionEnum,
        pub addString1: String,
        pub addString2: String,
        pub shouldSetBit0AndBit2: i32,
        pub u: Option<super::Union::Union>,
        pub shouldBeConstS1: Option<super::Union::Union>,
        pub defaultWithFoo: super::IntEnum::IntEnum,
    }
    impl Default for StructuredParcelable {
        fn default() -> Self {
            Self {
                shouldContainThreeFs: Default::default(),
                f: Default::default(),
                shouldBeJerry: Default::default(),
                shouldBeByteBar: Default::default(),
                shouldBeIntBar: Default::default(),
                shouldBeLongBar: Default::default(),
                shouldContainTwoByteFoos: Default::default(),
                shouldContainTwoIntFoos: Default::default(),
                shouldContainTwoLongFoos: Default::default(),
                stringDefaultsToFoo: "foo".into(),
                byteDefaultsToFour: 4,
                intDefaultsToFive: 5,
                longDefaultsToNegativeSeven: -7,
                booleanDefaultsToTrue: true,
                charDefaultsToC: '\'' as u16,
                floatDefaultsToPi: 3.14f32,
                doubleWithDefault: -314000000000000000f64,
                arrayDefaultsTo123: vec![1,2,3,],
                arrayDefaultsToEmpty: Default::default(),
                boolDefault: Default::default(),
                byteDefault: Default::default(),
                intDefault: Default::default(),
                longDefault: Default::default(),
                floatDefault: Default::default(),
                doubleDefault: Default::default(),
                checkDoubleFromFloat: 3.14f64,
                checkStringArray1: vec!["a".into(),"b".into(),],
                checkStringArray2: vec!["a".into(),"b".into(),],
                int32_min: -2147483648,
                int32_max: 2147483647,
                int64_max: 9223372036854775807,
                hexInt32_neg_1: -1,
                ibinder: Default::default(),
                empty: Default::default(),
                int8_1: vec![1,1,1,1,1,],
                int32_1: vec![1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,],
                int64_1: vec![1,1,1,1,1,1,1,1,1,1,],
                hexInt32_pos_1: 1,
                hexInt64_pos_1: 1,
                const_exprs_1: Default::default(),
                const_exprs_2: Default::default(),
                const_exprs_3: Default::default(),
                const_exprs_4: Default::default(),
                const_exprs_5: Default::default(),
                const_exprs_6: Default::default(),
                const_exprs_7: Default::default(),
                const_exprs_8: Default::default(),
                const_exprs_9: Default::default(),
                const_exprs_10: Default::default(),
                addString1: "hello world!".into(),
                addString2: "The quick brown fox jumps over the lazy dog.".into(),
                shouldSetBit0AndBit2: Default::default(),
                u: Default::default(),
                shouldBeConstS1: Default::default(),
                defaultWithFoo: super::IntEnum::IntEnum::FOO,
            }
        }
    }
    impl rsbinder::Parcelable for StructuredParcelable {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                _sub_parcel.write(&self.shouldContainThreeFs)?;
                _sub_parcel.write(&self.f)?;
                _sub_parcel.write(&self.shouldBeJerry)?;
                _sub_parcel.write(&self.shouldBeByteBar)?;
                _sub_parcel.write(&self.shouldBeIntBar)?;
                _sub_parcel.write(&self.shouldBeLongBar)?;
                _sub_parcel.write(&self.shouldContainTwoByteFoos)?;
                _sub_parcel.write(&self.shouldContainTwoIntFoos)?;
                _sub_parcel.write(&self.shouldContainTwoLongFoos)?;
                _sub_parcel.write(&self.stringDefaultsToFoo)?;
                _sub_parcel.write(&self.byteDefaultsToFour)?;
                _sub_parcel.write(&self.intDefaultsToFive)?;
                _sub_parcel.write(&self.longDefaultsToNegativeSeven)?;
                _sub_parcel.write(&self.booleanDefaultsToTrue)?;
                _sub_parcel.write(&self.charDefaultsToC)?;
                _sub_parcel.write(&self.floatDefaultsToPi)?;
                _sub_parcel.write(&self.doubleWithDefault)?;
                _sub_parcel.write(&self.arrayDefaultsTo123)?;
                _sub_parcel.write(&self.arrayDefaultsToEmpty)?;
                _sub_parcel.write(&self.boolDefault)?;
                _sub_parcel.write(&self.byteDefault)?;
                _sub_parcel.write(&self.intDefault)?;
                _sub_parcel.write(&self.longDefault)?;
                _sub_parcel.write(&self.floatDefault)?;
                _sub_parcel.write(&self.doubleDefault)?;
                _sub_parcel.write(&self.checkDoubleFromFloat)?;
                _sub_parcel.write(&self.checkStringArray1)?;
                _sub_parcel.write(&self.checkStringArray2)?;
                _sub_parcel.write(&self.int32_min)?;
                _sub_parcel.write(&self.int32_max)?;
                _sub_parcel.write(&self.int64_max)?;
                _sub_parcel.write(&self.hexInt32_neg_1)?;
                _sub_parcel.write(&self.ibinder)?;
                _sub_parcel.write(&self.empty)?;
                _sub_parcel.write(&self.int8_1)?;
                _sub_parcel.write(&self.int32_1)?;
                _sub_parcel.write(&self.int64_1)?;
                _sub_parcel.write(&self.hexInt32_pos_1)?;
                _sub_parcel.write(&self.hexInt64_pos_1)?;
                _sub_parcel.write(&self.const_exprs_1)?;
                _sub_parcel.write(&self.const_exprs_2)?;
                _sub_parcel.write(&self.const_exprs_3)?;
                _sub_parcel.write(&self.const_exprs_4)?;
                _sub_parcel.write(&self.const_exprs_5)?;
                _sub_parcel.write(&self.const_exprs_6)?;
                _sub_parcel.write(&self.const_exprs_7)?;
                _sub_parcel.write(&self.const_exprs_8)?;
                _sub_parcel.write(&self.const_exprs_9)?;
                _sub_parcel.write(&self.const_exprs_10)?;
                _sub_parcel.write(&self.addString1)?;
                _sub_parcel.write(&self.addString2)?;
                _sub_parcel.write(&self.shouldSetBit0AndBit2)?;
                _sub_parcel.write(&self.u)?;
                _sub_parcel.write(&self.shouldBeConstS1)?;
                _sub_parcel.write(&self.defaultWithFoo)?;
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                self.shouldContainThreeFs = _sub_parcel.read()?;
                self.f = _sub_parcel.read()?;
                self.shouldBeJerry = _sub_parcel.read()?;
                self.shouldBeByteBar = _sub_parcel.read()?;
                self.shouldBeIntBar = _sub_parcel.read()?;
                self.shouldBeLongBar = _sub_parcel.read()?;
                self.shouldContainTwoByteFoos = _sub_parcel.read()?;
                self.shouldContainTwoIntFoos = _sub_parcel.read()?;
                self.shouldContainTwoLongFoos = _sub_parcel.read()?;
                self.stringDefaultsToFoo = _sub_parcel.read()?;
                self.byteDefaultsToFour = _sub_parcel.read()?;
                self.intDefaultsToFive = _sub_parcel.read()?;
                self.longDefaultsToNegativeSeven = _sub_parcel.read()?;
                self.booleanDefaultsToTrue = _sub_parcel.read()?;
                self.charDefaultsToC = _sub_parcel.read()?;
                self.floatDefaultsToPi = _sub_parcel.read()?;
                self.doubleWithDefault = _sub_parcel.read()?;
                self.arrayDefaultsTo123 = _sub_parcel.read()?;
                self.arrayDefaultsToEmpty = _sub_parcel.read()?;
                self.boolDefault = _sub_parcel.read()?;
                self.byteDefault = _sub_parcel.read()?;
                self.intDefault = _sub_parcel.read()?;
                self.longDefault = _sub_parcel.read()?;
                self.floatDefault = _sub_parcel.read()?;
                self.doubleDefault = _sub_parcel.read()?;
                self.checkDoubleFromFloat = _sub_parcel.read()?;
                self.checkStringArray1 = _sub_parcel.read()?;
                self.checkStringArray2 = _sub_parcel.read()?;
                self.int32_min = _sub_parcel.read()?;
                self.int32_max = _sub_parcel.read()?;
                self.int64_max = _sub_parcel.read()?;
                self.hexInt32_neg_1 = _sub_parcel.read()?;
                self.ibinder = _sub_parcel.read()?;
                self.empty = _sub_parcel.read()?;
                self.int8_1 = _sub_parcel.read()?;
                self.int32_1 = _sub_parcel.read()?;
                self.int64_1 = _sub_parcel.read()?;
                self.hexInt32_pos_1 = _sub_parcel.read()?;
                self.hexInt64_pos_1 = _sub_parcel.read()?;
                self.const_exprs_1 = _sub_parcel.read()?;
                self.const_exprs_2 = _sub_parcel.read()?;
                self.const_exprs_3 = _sub_parcel.read()?;
                self.const_exprs_4 = _sub_parcel.read()?;
                self.const_exprs_5 = _sub_parcel.read()?;
                self.const_exprs_6 = _sub_parcel.read()?;
                self.const_exprs_7 = _sub_parcel.read()?;
                self.const_exprs_8 = _sub_parcel.read()?;
                self.const_exprs_9 = _sub_parcel.read()?;
                self.const_exprs_10 = _sub_parcel.read()?;
                self.addString1 = _sub_parcel.read()?;
                self.addString2 = _sub_parcel.read()?;
                self.shouldSetBit0AndBit2 = _sub_parcel.read()?;
                self.u = _sub_parcel.read()?;
                self.shouldBeConstS1 = _sub_parcel.read()?;
                self.defaultWithFoo = _sub_parcel.read()?;
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
        ConstantExpressionEnum : [i32; 10] {
            decInt32_1 = 1,
            decInt32_2 = 1,
            decInt64_1 = 1,
            decInt64_2 = 1,
            decInt64_3 = 1,
            decInt64_4 = 1,
            hexInt32_1 = 1,
            hexInt32_2 = 1,
            hexInt32_3 = 1,
            hexInt64_1 = 1,
        }
    }
}
pub mod ByteEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        ByteEnum : [i8; 3] {
            FOO = 1,
            BAR = 2,
            BAZ = 3,
        }
    }
}
pub mod IntEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        IntEnum : [i32; 4] {
            FOO = 1000,
            BAR = 2000,
            BAZ = 2001,
            QUX = 2002,
        }
    }
}
pub mod LongEnum {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        LongEnum : [i64; 3] {
            FOO = 100000000000,
            BAR = 200000000000,
            BAZ = 200000000001,
        }
    }
}
pub mod Union {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    #[derive(Clone,PartialEq)]
    pub enum Union {
        Ns(Vec<i32>),
        N(i32),
        M(i32),
        S(String),
        Ibinder(Option<rsbinder::SIBinder>),
        Ss(Vec<String>),
        Be(super::ByteEnum::ByteEnum),
    }
    pub const S1: &str = "a string constant in union";
    impl Default for Union {
        fn default() -> Self {
            Self::Ns(Default::default())
        }
    }
    impl rsbinder::Parcelable for Union {
        fn write_to_parcel(&self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match self {
                Self::Ns(v) => {
                    parcel.write(&0i32)?;
                    parcel.write(v)
                }
                Self::N(v) => {
                    parcel.write(&1i32)?;
                    parcel.write(v)
                }
                Self::M(v) => {
                    parcel.write(&2i32)?;
                    parcel.write(v)
                }
                Self::S(v) => {
                    parcel.write(&3i32)?;
                    parcel.write(v)
                }
                Self::Ibinder(v) => {
                    parcel.write(&4i32)?;
                    parcel.write(v)
                }
                Self::Ss(v) => {
                    parcel.write(&5i32)?;
                    parcel.write(v)
                }
                Self::Be(v) => {
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
                    *self = Self::Ns(value);
                    Ok(())
                }
                1 => {
                    let value: i32 = parcel.read()?;
                    *self = Self::N(value);
                    Ok(())
                }
                2 => {
                    let value: i32 = parcel.read()?;
                    *self = Self::M(value);
                    Ok(())
                }
                3 => {
                    let value: String = parcel.read()?;
                    *self = Self::S(value);
                    Ok(())
                }
                4 => {
                    let value: Option<rsbinder::SIBinder> = parcel.read()?;
                    *self = Self::Ibinder(value);
                    Ok(())
                }
                5 => {
                    let value: Vec<String> = parcel.read()?;
                    *self = Self::Ss(value);
                    Ok(())
                }
                6 => {
                    let value: super::ByteEnum::ByteEnum = parcel.read()?;
                    *self = Self::Be(value);
                    Ok(())
                }
                _ => Err(rsbinder::StatusCode::BadValue),
            }
        }
    }
    rsbinder::impl_serialize_for_parcelable!(Union);
    rsbinder::impl_deserialize_for_parcelable!(Union);
    impl rsbinder::ParcelableMetadata for Union {
        fn descriptor() -> &'static str { "android.aidl.tests.Union" }
    }
    rsbinder::declare_binder_enum! {
        Tag : [i32; 7] {
            ns = 0,
            n = 1,
            m = 2,
            s = 3,
            ibinder = 4,
            ss = 5,
            be = 6,
        }
    }
}
        "#)?;
    Ok(())
}