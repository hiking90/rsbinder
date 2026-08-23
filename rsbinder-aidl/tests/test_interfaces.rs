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
            fn r#methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::BinderResult<Option<Vec<Option<String>>>>;
            fn getDefaultImpl() -> Option<IMyInterfaceDefaultRef> where Self: Sized {
                DEFAULT_IMPL.get().cloned()
            }
            fn setDefaultImpl(d: IMyInterfaceDefaultRef) -> IMyInterfaceDefaultRef where Self: Sized {
                DEFAULT_IMPL.get_or_init(|| d).clone()
            }
        }
        pub trait IMyInterfaceDefault: Send + Sync {
            fn r#methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::BinderResult<Option<Vec<Option<String>>>> {
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
                let mut data = self.binder.as_remote().ok_or(rsbinder::StatusCode::BadType)?.prepare_transact(true)?;
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
            fn read_response_methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _aidl_reply: rsbinder::Result<Option<rsbinder::Parcel>>) -> rsbinder::BinderResult<Option<Vec<Option<String>>>> {
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
            fn r#methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::BinderResult<Option<Vec<Option<String>>>> {
                let _aidl_data = self.build_parcel_methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_array_in, _arg_iface_array_out, _arg_iface_array_inout, _arg_nullable_iface_array_in, _arg_nullable_iface_array_out, _arg_nullable_iface_array_inout)?;
                let _aidl_reply = self.binder.as_remote().ok_or(rsbinder::StatusCode::BadType)?.submit_transact(transactions::r#methodWithInterfaces, &_aidl_data, rsbinder::FLAG_CLEAR_BUF | rsbinder::FLAG_PRIVATE_LOCAL);
                self.read_response_methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_array_in, _arg_iface_array_out, _arg_iface_array_inout, _arg_nullable_iface_array_in, _arg_nullable_iface_array_out, _arg_nullable_iface_array_inout, _aidl_reply)
            }
        }
        impl IMyInterface for rsbinder::Binder<BnMyInterface> {
            fn r#methodWithInterfaces(&self, _arg_iface: &rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>, _arg_nullable_iface: Option<&rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_iface_array_in: &[rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>], _arg_iface_array_out: &mut Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>, _arg_iface_array_inout: &mut Vec<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>, _arg_nullable_iface_array_in: Option<&[Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>]>, _arg_nullable_iface_array_out: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>, _arg_nullable_iface_array_inout: &mut Option<Vec<Option<rsbinder::Strong<dyn super::IEmptyInterface::IEmptyInterface>>>>) -> rsbinder::BinderResult<Option<Vec<Option<String>>>> {
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

/// An interface that names *itself* stays `Strong<dyn IFoo>`: the box guard is
/// for infinitely-sized parcelable fields, and `dyn Box<IFoo>` is not a trait.
#[test]
fn self_referencing_interface_is_not_boxed() -> Result<(), Box<dyn Error>> {
    let ctx = rsbinder_aidl::SourceContext::new(
        "test.aidl",
        r##"
interface ISelfRef {
    void register(in ISelfRef cb);
    ISelfRef fetch();
    @nullable ISelfRef maybe();
    void many(in ISelfRef[] cbs);
}
        "##,
    );
    let document = rsbinder_aidl::parse_document(&ctx)?;
    let out = rsbinder_aidl::Generator::new(false, false)
        .document(&document)?
        .1;

    assert!(
        !out.contains("Box<"),
        "a binder handle needs no box:\n{out}"
    );
    assert!(out.contains("rsbinder::Strong<dyn ISelfRef>"), "{out}");
    assert!(
        out.contains("Option<rsbinder::Strong<dyn ISelfRef>>"),
        "@nullable must still be an Option:\n{out}"
    );
    assert!(
        out.contains("Vec<rsbinder::Strong<dyn ISelfRef>>"),
        "arrays must still be Vec:\n{out}"
    );
    // `Box<…>` is syntactically valid, so parsing alone would not catch it —
    // the assertion above is the guard; this only rules out other breakage.
    syn::parse_file(&out).map_err(|e| format!("generated code does not parse: {e}\n{out}"))?;
    Ok(())
}

/// The counterpart the box guard exists for: a parcelable naming itself is
/// infinitely sized without one, so this must keep boxing.
#[test]
fn self_referencing_parcelable_is_still_boxed() -> Result<(), Box<dyn Error>> {
    let ctx = rsbinder_aidl::SourceContext::new(
        "test.aidl",
        r##"
parcelable Node {
    int value;
    @nullable Node next;
}
        "##,
    );
    let document = rsbinder_aidl::parse_document(&ctx)?;
    let out = rsbinder_aidl::Generator::new(false, false)
        .document(&document)?
        .1;

    assert!(out.contains("Option<Box<Node>>"), "{out}");
    syn::parse_file(&out).map_err(|e| format!("generated code does not parse: {e}\n{out}"))?;
    Ok(())
}

/// Two parcelables that reference each other form a cycle just as a
/// self-reference does, so the field that closes it needs the same box.
#[test]
fn mutually_recursive_parcelables_are_boxed() -> Result<(), Box<dyn Error>> {
    let ctx = rsbinder_aidl::SourceContext::new(
        "test.aidl",
        r##"
parcelable Branch {
    int value;
    @nullable Leaf leaf;
}
parcelable Leaf {
    int value;
    @nullable Branch parent;
}
        "##,
    );
    let document = rsbinder_aidl::parse_document(&ctx)?;
    let out = rsbinder_aidl::Generator::new(false, false)
        .document(&document)?
        .1;

    assert!(out.contains("Option<Box<super::Leaf::Leaf>>"), "{out}");
    assert!(out.contains("Option<Box<super::Branch::Branch>>"), "{out}");
    syn::parse_file(&out).map_err(|e| format!("generated code does not parse: {e}\n{out}"))?;
    Ok(())
}

/// A parcelable that merely *uses* another one is not a cycle, so nothing is
/// boxed — the guard must not fire on every cross-reference.
#[test]
fn acyclic_parcelable_reference_is_not_boxed() -> Result<(), Box<dyn Error>> {
    let ctx = rsbinder_aidl::SourceContext::new(
        "test.aidl",
        r##"
parcelable Outer {
    Inner inner;
}
parcelable Inner {
    int value;
}
        "##,
    );
    let document = rsbinder_aidl::parse_document(&ctx)?;
    let out = rsbinder_aidl::Generator::new(false, false)
        .document(&document)?
        .1;

    assert!(!out.contains("Box<"), "{out}");
    syn::parse_file(&out).map_err(|e| format!("generated code does not parse: {e}\n{out}"))?;
    Ok(())
}

/// `InterfaceRender::new` takes the unescaped name: escaping before the call
/// would put `r#` inside `Bn`/`Bp`, which does not lex.
#[test]
fn render_constructors_escape_rust_keywords() -> Result<(), Box<dyn Error>> {
    use rsbinder_aidl::render::{EnumRender, InterfaceRender, ParcelableRender};

    let i = InterfaceRender::new("type", "test.type");
    assert_eq!(i.name, "r#type");
    assert_eq!(i.module, "r#type");
    assert_eq!(i.bn_name, "Bntype");
    assert_eq!(i.bp_name, "Bptype");

    let p = ParcelableRender::new("type", "test.type");
    assert_eq!(p.name, "r#type");
    assert_eq!(p.module, "r#type");

    // The enum template writes the name through `declare_binder_enum!`, which
    // prefixes `r#` itself.
    let e = EnumRender::new("type", "i32");
    assert_eq!(e.name, "type");
    assert_eq!(e.module, "r#type");
    Ok(())
}

/// A cycle that runs through an *interface* is finite — `Strong<dyn …>` is a
/// handle — so nothing may be boxed. This is the AOSP `CircularParcelable` /
/// `ITestService` shape.
#[test]
fn cycle_through_an_interface_is_not_boxed() -> Result<(), Box<dyn Error>> {
    let ctx = rsbinder_aidl::SourceContext::new(
        "test.aidl",
        r##"
parcelable Circular {
    @nullable IRing ring;
}
interface IRing {
    IRing get(out Circular c);
}
        "##,
    );
    let document = rsbinder_aidl::parse_document(&ctx)?;
    let out = rsbinder_aidl::Generator::new(false, false)
        .document(&document)?
        .1;

    assert!(
        !out.contains("Box<"),
        "an interface handle breaks the cycle:\n{out}"
    );
    syn::parse_file(&out).map_err(|e| format!("generated code does not parse: {e}\n{out}"))?;
    Ok(())
}

/// A `Vec` element is a fixed-size handle whatever it holds, so an array
/// member neither closes a sizing cycle nor takes a box — and it must not,
/// because `Box<T>` implements no array codec, so `Vec<Box<T>>` would emit
/// code that does not compile.
#[test]
fn a_cycle_through_an_array_is_not_boxed() -> Result<(), Box<dyn Error>> {
    let ctx = rsbinder_aidl::SourceContext::new(
        "test.aidl",
        r##"
parcelable Tree {
    Node[] nodes;
}
parcelable Node {
    @nullable Tree owner;
}
        "##,
    );
    let document = rsbinder_aidl::parse_document(&ctx)?;
    let out = rsbinder_aidl::Generator::new(false, false)
        .document(&document)?
        .1;

    assert!(
        !out.contains("Box<"),
        "a Vec already breaks the cycle:\n{out}"
    );
    assert!(out.contains("Vec<super::Node::Node>"), "{out}");
    assert!(out.contains("Option<super::Tree::Tree>"), "{out}");
    syn::parse_file(&out).map_err(|e| format!("generated code does not parse: {e}\n{out}"))?;
    Ok(())
}

/// A `<Union>.Tag` is the scalar `declare_binder_enum!` newtype, not the union
/// it names — it cannot hold the enclosing declaration, so it is never boxed.
/// Its lookup reports the parent union's namespace, which is exactly the trap.
#[test]
fn a_union_tag_field_is_not_boxed() -> Result<(), Box<dyn Error>> {
    let ctx = rsbinder_aidl::SourceContext::new(
        "test.aidl",
        r##"
union U {
    int a = 0;
    @nullable Node n;
}
parcelable Node {
    U.Tag tag;
}
        "##,
    );
    let document = rsbinder_aidl::parse_document(&ctx)?;
    let out = rsbinder_aidl::Generator::new(false, false)
        .document(&document)?
        .1;

    assert!(!out.contains("Box<"), "a Tag holds no union:\n{out}");
    assert!(out.contains("super::U::Tag"), "{out}");
    syn::parse_file(&out).map_err(|e| format!("generated code does not parse: {e}\n{out}"))?;
    Ok(())
}

/// A non-nullable cycle has no terminating form: boxing it alone would give
/// the generated `Default` — the deserialization entry point — infinite
/// recursion, so it must be a diagnostic rather than a runtime abort.
#[test]
fn a_non_nullable_cycle_is_rejected() {
    aidl_generator_should_fail(
        r##"
parcelable Branch {
    Leaf leaf;
}
parcelable Leaf {
    Branch parent;
}
        "##,
        "closes a reference cycle",
    );
    aidl_generator_should_fail(
        r##"
parcelable Node {
    Node next;
}
        "##,
        "closes a reference cycle",
    );
}

/// A fixed-size array keeps its elements inline, so it closes a cycle that a
/// variable-length one would not — and no box can rescue it.
#[test]
fn a_fixed_size_array_cycle_is_rejected() {
    aidl_generator_should_fail(
        r##"
parcelable Tree {
    Node[3] nodes;
}
parcelable Node {
    @nullable Tree owner;
}
        "##,
        "closes a reference cycle",
    );
}
