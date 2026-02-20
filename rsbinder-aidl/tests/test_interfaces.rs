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

#[test]
fn test_array_of_interfaces_check() -> Result<(), Box<dyn Error>> {
    aidl_generator(
        r##"
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
            fn getDefaultImpl() -> Option<IEmptyInterfaceDefaultRef> where Self: Sized {
                DEFAULT_IMPL.get().cloned()
            }
            fn setDefaultImpl(d: IEmptyInterfaceDefaultRef) -> IEmptyInterfaceDefaultRef where Self: Sized {
                DEFAULT_IMPL.get_or_init(|| d).clone()
            }
        }
        pub trait IEmptyInterfaceDefault: Send + Sync {
        }
        pub(crate) mod transactions {
        }
        pub type IEmptyInterfaceDefaultRef = std::sync::Arc<dyn IEmptyInterfaceDefault>;
        static DEFAULT_IMPL: std::sync::OnceLock<IEmptyInterfaceDefaultRef> = std::sync::OnceLock::new();
        rsbinder::declare_binder_interface! {
            IEmptyInterface["ArrayOfInterfaces.IEmptyInterface"] {
                native: {
                    BnEmptyInterface(on_transact),
                },
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
            _service: &dyn IEmptyInterface, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match _code {
                _ => Err(rsbinder::StatusCode::UnknownTransaction),
            }
        }
    }
    pub mod IMyInterface {
        #![allow(non_upper_case_globals, non_snake_case, dead_code)]
        pub trait IMyInterface: rsbinder::Interface + Send {
            fn descriptor() -> &'static str where Self: Sized { "ArrayOfInterfaces.IMyInterface" }
            fn r#methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::status::Result<Option<Vec<Option<String>>>>;
            fn getDefaultImpl() -> Option<IMyInterfaceDefaultRef> where Self: Sized {
                DEFAULT_IMPL.get().cloned()
            }
            fn setDefaultImpl(d: IMyInterfaceDefaultRef) -> IMyInterfaceDefaultRef where Self: Sized {
                DEFAULT_IMPL.get_or_init(|| d).clone()
            }
        }
        pub trait IMyInterfaceDefault: Send + Sync {
            fn r#methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::status::Result<Option<Vec<Option<String>>>> {
                Err(rsbinder::StatusCode::UnknownTransaction.into())
            }
        }
        pub(crate) mod transactions {
            pub(crate) const r#methodWithInterfaces: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;
        }
        pub type IMyInterfaceDefaultRef = std::sync::Arc<dyn IMyInterfaceDefault>;
        static DEFAULT_IMPL: std::sync::OnceLock<IMyInterfaceDefaultRef> = std::sync::OnceLock::new();
        rsbinder::declare_binder_interface! {
            IMyInterface["ArrayOfInterfaces.IMyInterface"] {
                native: {
                    BnMyInterface(on_transact),
                },
                proxy: BpMyInterface,
            }
        }
        impl BpMyInterface {
            fn build_parcel_methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::Result<rsbinder::Parcel> {
                let mut data = self.binder.as_proxy().unwrap().prepare_transact(true)?;
                data.write(_arg_iface)?;
                data.write(&_arg_nullable_iface)?;
                data.write(_arg_iface_array_in)?;
                data.write_slice_size(Some(_arg_iface_array_out))?;
                data.write(_arg_iface_array_inout)?;
                data.write(&_arg_nullable_iface_array_in)?;
                data.write_slice_size(_arg_nullable_iface_array_out.as_deref())?;
                data.write(_arg_nullable_iface_array_inout)?;
                Ok(data)
            }
            fn read_response_methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _aidl_reply: rsbinder::Result<Option<rsbinder::Parcel>>) -> rsbinder::status::Result<Option<Vec<Option<String>>>> {
                if let Err(rsbinder::StatusCode::UnknownTransaction) = _aidl_reply {
                    if let Some(_aidl_default_impl) = <Self as IMyInterface>::getDefaultImpl() {
                      return _aidl_default_impl.r#methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_array_in, _arg_iface_array_out, _arg_iface_array_inout, _arg_nullable_iface_array_in, _arg_nullable_iface_array_out, _arg_nullable_iface_array_inout);
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
            fn r#methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::status::Result<Option<Vec<Option<String>>>> {
                let _aidl_data = self.build_parcel_methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_array_in, _arg_iface_array_out, _arg_iface_array_inout, _arg_nullable_iface_array_in, _arg_nullable_iface_array_out, _arg_nullable_iface_array_inout)?;
                let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::r#methodWithInterfaces, &_aidl_data, rsbinder::FLAG_CLEAR_BUF);
                self.read_response_methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_array_in, _arg_iface_array_out, _arg_iface_array_inout, _arg_nullable_iface_array_in, _arg_nullable_iface_array_out, _arg_nullable_iface_array_inout, _aidl_reply)
            }
        }
        impl IMyInterface for rsbinder::Binder<BnMyInterface> {
            fn r#methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::status::Result<Option<Vec<Option<String>>>> {
                self.0.r#methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_array_in, _arg_iface_array_out, _arg_iface_array_inout, _arg_nullable_iface_array_in, _arg_nullable_iface_array_out, _arg_nullable_iface_array_inout)
            }
        }
        fn on_transact(
            _service: &dyn IMyInterface, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match _code {
                transactions::r#methodWithInterfaces => {
                    let _arg_iface: rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface> = _reader.read()?;
                    let _arg_nullable_iface: Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>> = _reader.read()?;
                    let _arg_iface_array_in: Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>> = _reader.read()?;
                    let mut _arg_iface_array_out: Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>> = Default::default();
                    _reader.resize_out_vec(&mut _arg_iface_array_out)?;
                    let mut _arg_iface_array_inout: Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>> = _reader.read()?;
                    let _arg_nullable_iface_array_in: Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>> = _reader.read()?;
                    let mut _arg_nullable_iface_array_out: Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>> = Default::default();
                    _reader.resize_nullable_out_vec(&mut _arg_nullable_iface_array_out)?;
                    let mut _arg_nullable_iface_array_inout: Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>> = _reader.read()?;
                    let _aidl_return = _service.r#methodWithInterfaces(&_arg_iface, _arg_nullable_iface.as_ref(), &_arg_iface_array_in, &mut _arg_iface_array_out, &mut _arg_iface_array_inout, _arg_nullable_iface_array_in.as_deref(), &mut _arg_nullable_iface_array_out, &mut _arg_nullable_iface_array_inout);
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
        "##,
    )
}

/// Helper to verify that generated code contains a specific string fragment
fn aidl_generator_contains(input: &str, expected_fragment: &str) -> Result<(), Box<dyn Error>> {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx)?;
    let gen = rsbinder_aidl::Generator::new(false, false);
    let res = gen.document(&document)?;
    assert!(
        res.1.contains(expected_fragment),
        "Generated code does not contain expected fragment.\nExpected fragment:\n{}\n\nGenerated code:\n{}",
        expected_fragment, res.1
    );
    Ok(())
}

/// Helper to verify that AIDL parsing + code generation returns an error
fn aidl_generator_should_fail(input: &str, expected_error_substring: &str) {
    let result = (|| -> Result<(), Box<dyn Error>> {
        let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
        let document = rsbinder_aidl::parse_document(&ctx)?;
        let gen = rsbinder_aidl::Generator::new(false, false);
        gen.document(&document)?;
        Ok(())
    })();
    assert!(result.is_err(), "Expected error but got success");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains(expected_error_substring),
        "Error message '{}' does not contain '{}'",
        err_msg,
        expected_error_substring
    );
}

#[test]
fn test_explicit_transaction_codes() -> Result<(), Box<dyn Error>> {
    let input = r#"
interface IExplicit {
    void method1() = 10;
    void method2() = 20;
}
    "#;
    aidl_generator_contains(input,
        "pub(crate) const r#method1: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 10;")?;
    aidl_generator_contains(input,
        "pub(crate) const r#method2: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 20;")?;
    Ok(())
}

#[test]
fn test_explicit_transaction_code_zero() -> Result<(), Box<dyn Error>> {
    let input = r#"
interface IZero {
    void method1() = 0;
    void method2() = 1;
}
    "#;
    aidl_generator_contains(input,
        "pub(crate) const r#method1: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;")?;
    aidl_generator_contains(input,
        "pub(crate) const r#method2: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 1;")?;
    Ok(())
}

#[test]
fn test_mixed_transaction_codes_error() {
    aidl_generator_should_fail(
        r#"
interface IMixed {
    void method1() = 10;
    void method2();
}
        "#,
        "mixed explicit/implicit transaction IDs",
    );
}

#[test]
fn test_implicit_transaction_codes_unchanged() -> Result<(), Box<dyn Error>> {
    let input = r#"
interface IImplicit {
    void method1();
    void method2();
}
    "#;
    aidl_generator_contains(input,
        "pub(crate) const r#method1: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;")?;
    aidl_generator_contains(input,
        "pub(crate) const r#method2: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 1;")?;
    Ok(())
}

#[test]
fn test_duplicate_transaction_codes_error() {
    aidl_generator_should_fail(
        r#"
interface IDuplicate {
    void method1() = 10;
    void method2() = 10;
}
        "#,
        "transaction code 10 conflict between",
    );
}
