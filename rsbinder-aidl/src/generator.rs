// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::error::Error;
use serde::{Serialize, Deserialize};

use tera::Tera;

use crate::{parser, add_indent, Namespace};
use crate::parser::Direction;

const ENUM_TEMPLATE: &str = r##"
pub mod {{mod}} {
    #![allow(non_upper_case_globals, non_snake_case)]
    rsbinder::declare_binder_enum! {
        r#{{enum_name}} : [{{enum_type}}; {{enum_len}}] {
    {%- for member in members %}
            r#{{ member.0 }} = {{ member.1 }},
    {%- endfor %}
        }
    }
}
"##;

const UNION_TEMPLATE: &str = r#"
pub mod {{mod}} {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    #[derive(Debug)]
    {%- if derive|length > 0 %}
    #[derive({{ derive }})]
    {%- endif %}
    pub enum r#{{union_name}} {
    {%- for member in members %}
        r#{{ member.0 }}({{ member.1 }}),
    {%- endfor %}
    }
    {%- for member in const_members %}
    pub const r#{{ member.0 }}: {{ member.1 }} = {{ member.2 }};
    {%- endfor %}
    impl Default for r#{{union_name}} {
        fn default() -> Self {
    {%- if members|length > 0 %}
            Self::{{members[0][0]}}({{members[0][3]}})
    {%- endif %}
        }
    }
    impl rsbinder::Parcelable for r#{{union_name}} {
        fn write_to_parcel(&self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match self {
    {%- set counter = 0 %}
    {%- for member in members %}
                Self::r#{{member.0}}(v) => {
                    parcel.write(&{{counter}}i32)?;
                    parcel.write(v)
                }
    {%- set_global counter = counter + 1 %}
    {%- endfor %}
            }
        }
        fn read_from_parcel(&mut self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            let tag: i32 = parcel.read()?;
            match tag {
    {%- set counter = 0 %}
    {%- for member in members %}
                {{counter}} => {
                    let value: {{member.1}} = parcel.read()?;
                    *self = Self::r#{{member.0}}(value);
                    Ok(())
                }
    {%- set_global counter = counter + 1 %}
    {%- endfor %}
                _ => Err(rsbinder::StatusCode::BadValue),
            }
        }
    }
    rsbinder::impl_serialize_for_parcelable!(r#{{union_name}});
    rsbinder::impl_deserialize_for_parcelable!(r#{{union_name}});
    impl rsbinder::ParcelableMetadata for r#{{union_name}} {
        fn descriptor() -> &'static str { "{{ namespace }}" }
    }
    rsbinder::declare_binder_enum! {
        Tag : [i32; {{ members|length }}] {
    {%- set counter = 0 %}
    {%- for member in members %}
            r#{{ member.2 }} = {{ counter }},
    {%- set_global counter = counter + 1 %}
    {%- endfor %}
        }
    }
}
"#;

const PARCELABLE_TEMPLATE: &str = r#"
pub mod {{mod}} {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    {%- for member in const_members %}
    pub const r#{{ member.0 }}: {{ member.1 }} = {{ member.2 }};
    {%- endfor %}
    #[derive(Debug)]
    {%- if derive|length > 0 %}
    #[derive({{ derive }})]
    {%- endif %}
    pub struct {{name}} {
    {%- for member in members %}
        pub r#{{ member.0 }}: {{ member.1 }},
    {%- endfor %}
    }
    impl Default for {{ name }} {
        fn default() -> Self {
            Self {
            {%- for member in members %}
                r#{{ member.0 }}: {{ member.2 }},
            {%- endfor %}
            }
        }
    }
    impl rsbinder::Parcelable for {{name}} {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                {%- for member in members %}
                _sub_parcel.write(&self.r#{{ member.0 }})?;
                {%- endfor %}
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                {%- for member in members %}
                self.r#{{ member.0 }} = _sub_parcel.read()?;
                {%- endfor %}
                Ok(())
            })
        }
    }
    rsbinder::impl_serialize_for_parcelable!({{name}});
    rsbinder::impl_deserialize_for_parcelable!({{name}});
    impl rsbinder::ParcelableMetadata for {{name}} {
        fn descriptor() -> &'static str { "{{namespace}}" }
    }
    {%- if nested|length>0 %}
    {{nested}}
    {%- endif %}
}
"#;

const INTERFACE_TEMPLATE: &str = r#"
pub mod {{mod}} {
    #![allow(non_upper_case_globals, non_snake_case, dead_code)]
    {%- for member in const_members %}
    pub const r#{{ member.0 }}: {{ member.1 }} = {{ member.2 }};
    {%- endfor %}
    pub trait {{name}}: rsbinder::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "{{ namespace }}" }
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}({{ member.args }}) -> rsbinder::status::Result<{{ member.return_type }}>;
        {%- endfor %}
        fn getDefaultImpl() -> {{ name }}DefaultRef where Self: Sized {
            DEFAULT_IMPL.lock().unwrap().clone()
        }
        fn setDefaultImpl(d: {{ name }}DefaultRef) -> {{ name }}DefaultRef where Self: Sized {
            std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
        }
    }
    {%- if enabled_async %}
    pub trait {{name}}Async<P>: rsbinder::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "{{ namespace }}" }
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}<'a>({{ member.args_async }}) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<{{ member.return_type }}>>;
        {%- endfor %}
    }
    #[::async_trait::async_trait]
    pub trait {{name}}AsyncService: rsbinder::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "{{ namespace }}" }
        {%- for member in fn_members %}
        async fn r#{{ member.identifier }}({{ member.args }}) -> rsbinder::status::Result<{{ member.return_type }}>;
        {%- endfor %}
    }
    impl {{bn_name}}
    {
        pub fn new_async_binder<T, R>(inner: T, rt: R) -> rsbinder::Strong<dyn {{name}}>
        where
            T: {{name}}AsyncService + Sync + Send + 'static,
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
            impl<T, R> {{bn_name}}Adapter for Wrapper<T, R>
            where
                T: {{name}}AsyncService + Sync + Send + 'static,
                R: rsbinder::BinderAsyncRuntime + Send + Sync + 'static,
            {
                fn as_sync(&self) -> &dyn {{name}} {
                    self
                }
                fn as_async(&self) -> &dyn {{name}}AsyncService {
                    &self._inner
                }
            }
            impl<T, R> {{name}} for Wrapper<T, R>
            where
                T: {{name}}AsyncService + Sync + Send + 'static,
                R: rsbinder::BinderAsyncRuntime + Send + Sync + 'static,
            {
                {%- for member in fn_members %}
                fn r#{{ member.identifier }}({{ member.args }}) -> rsbinder::status::Result<{{ member.return_type }}> {
                    self._rt.block_on(self._inner.r#{{ member.identifier }}({{ member.func_call_params }}))
                }
                {%- endfor %}
            }
            let wrapped = Wrapper { _inner: inner, _rt: rt };
            let binder = rsbinder::native::Binder::new_with_stability({{bn_name}}(Box::new(wrapped)), rsbinder::Stability::default());
            rsbinder::Strong::new(Box::new(binder))
        }
    }
    {%- endif %}
    pub trait {{ name }}Default: Send + Sync {
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}({{ member.args }}) -> rsbinder::status::Result<{{ member.return_type }}> {
            Err(rsbinder::StatusCode::UnknownTransaction.into())
        }
        {%- endfor %}
    }
    pub(crate) mod transactions {
        {%- set counter = 0 %}
        {%- for member in fn_members %}
        pub(crate) const r#{{ member.identifier }}: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + {{ counter }};
        {%- set_global counter = counter + 1 %}
        {%- endfor %}
    }
    pub type {{ name }}DefaultRef = Option<std::sync::Arc<dyn {{ name }}Default>>;
    use lazy_static::lazy_static;
    lazy_static! {
        static ref DEFAULT_IMPL: std::sync::Mutex<{{ name }}DefaultRef> = std::sync::Mutex::new(None);
    }
    rsbinder::declare_binder_interface! {
        {{ name }}["{{ namespace }}"] {
            native: {
                {{ bn_name }}(on_transact),
                {%- if enabled_async %}
                adapter: {{ bn_name }}Adapter,
                r#async: {{ name }}AsyncService,
                {%- endif %}
            },
            proxy: {{ bp_name }},
            {%- if enabled_async %}
            r#async: {{ name }}Async,
            {%- endif %}
        }
    }
    impl {{ bp_name }} {
        {%- for member in fn_members %}
        fn build_parcel_{{ member.identifier }}({{ member.args }}) -> rsbinder::Result<rsbinder::Parcel> {
            {%- if member.write_funcs|length > 0 %}
            let mut data = self.binder.as_proxy().unwrap().prepare_transact(true)?;
            {%- for func in member.write_funcs %}
            {{ func }}
            {%- endfor %}
            {%- else %}
            let data = self.binder.as_proxy().unwrap().prepare_transact(true)?;
            {%- endif %}
            Ok(data)
        }
        fn read_response_{{ member.identifier }}({{ member.args }}, _aidl_reply: rsbinder::Result<Option<rsbinder::Parcel>>) -> rsbinder::status::Result<{{ member.return_type }}> {
            {%- if oneway or member.oneway %}
            Ok(())
            {%- else %}
            if let Err(rsbinder::StatusCode::UnknownTransaction) = _aidl_reply {
                if let Some(_aidl_default_impl) = <Self as {{name}}>::getDefaultImpl() {
                  return _aidl_default_impl.r#{{ member.identifier }}({{ member.func_call_params }});
                }
            }
            let mut _aidl_reply = _aidl_reply?.ok_or(rsbinder::StatusCode::UnexpectedNull)?;
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            if !_status.is_ok() { return Err(_status); }
            {%- if member.return_type != "()" %}
            let _aidl_return: {{ member.return_type }} = _aidl_reply.read()?;
            {%- endif %}
            {%- for arg in member.read_onto_params %}
            _aidl_reply.read_onto({{ arg }})?;
            {%- endfor %}
            {%- if member.return_type != "()" %}
            Ok(_aidl_return)
            {%- else %}
            Ok(())
            {%- endif %}
            {%- endif %}
        }
        {%- endfor %}
    }
    impl {{ name }} for {{ bp_name }} {
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}({{ member.args }}) -> rsbinder::status::Result<{{ member.return_type }}> {
            let _aidl_data = self.build_parcel_{{ member.identifier }}({{ member.func_call_params }})?;
            let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::r#{{ member.identifier }}, &_aidl_data, {% if oneway or member.oneway %}rsbinder::FLAG_ONEWAY | {% endif %}rsbinder::FLAG_CLEAR_BUF);
            {%- if member.func_call_params|length > 0 %}
            self.read_response_{{ member.identifier }}({{ member.func_call_params }}, _aidl_reply)
            {%- else %}
            self.read_response_{{ member.identifier }}(_aidl_reply)
            {%- endif %}
        }
        {%- endfor %}
    }
    {%- if enabled_async %}
    impl<P: rsbinder::BinderAsyncPool> {{name}}Async<P> for {{ bp_name }} {
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}<'a>({{ member.args_async }}) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<{{ member.return_type }}>> {
            let _aidl_data = match self.build_parcel_{{ member.identifier }}({{ member.func_call_params }}) {
                Ok(_aidl_data) => _aidl_data,
                Err(err) => return Box::pin(std::future::ready(Err(err.into()))),
            };
            let binder = self.binder.clone();
            P::spawn(
                move || binder.as_proxy().unwrap().submit_transact(transactions::r#{{ member.identifier }}, &_aidl_data, rsbinder::FLAG_CLEAR_BUF | rsbinder::FLAG_PRIVATE_LOCAL),
                move |_aidl_reply| async move {
                    {%- if member.func_call_params|length > 0 %}
                    self.read_response_{{ member.identifier }}({{ member.func_call_params }}, _aidl_reply)
                    {%- else %}
                    self.read_response_{{ member.identifier }}(_aidl_reply)
                    {%- endif %}
                }
            )
        }
        {%- endfor %}
    }
    impl<P: rsbinder::BinderAsyncPool> {{name}}Async<P> for rsbinder::Binder<{{bn_name}}>
    {
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}<'a>({{ member.args_async }}) -> rsbinder::BoxFuture<'a, rsbinder::status::Result<{{ member.return_type }}>> {
            self.0.as_async().r#{{ member.identifier }}({{ member.func_call_params }})
        }
        {%- endfor %}
    }
    {%- endif %}
    impl {{ name }} for rsbinder::Binder<{{ bn_name }}> {
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}({{ member.args }}) -> rsbinder::status::Result<{{ member.return_type }}> {
            {%- if enabled_async %}
            self.0.as_sync().r#{{ member.identifier }}({{ member.func_call_params }})
            {%- else %}
            self.0.r#{{ member.identifier }}({{ member.func_call_params }})
            {%- endif %}
        }
        {%- endfor %}
    }
    fn on_transact(
        _service: &dyn {{ name }}, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
        match _code {
        {%- for member in fn_members %}
            transactions::r#{{ member.identifier }} => {
            {%- for decl in member.transaction_decls %}
                {{ decl }}
            {%- endfor %}
                let _aidl_return = _service.r#{{ member.identifier }}({{ member.transaction_params }});
            {%- if not oneway and not member.oneway %}
                match &_aidl_return {
                    Ok(_aidl_return) => {
                        _reply.write(&rsbinder::Status::from(rsbinder::StatusCode::Ok))?;
                        {%- if member.transaction_has_return %}
                        _reply.write(_aidl_return)?;
                        {%- endif %}
                        {%- for arg in member.transaction_write %}
                        _reply.write(&{{ arg }})?;
                        {%- endfor %}
                    }
                    Err(_aidl_status) => {
                        _reply.write(_aidl_status)?;
                    }
                }
            {%- endif %}
                Ok(())
            }
        {%- endfor %}
            _ => Err(rsbinder::StatusCode::UnknownTransaction),
        }
    }
    {%- if nested|length>0 %}
    {{nested}}
    {%- endif %}
}
"#;

lazy_static! {
    pub static ref TEMPLATES: Tera = {
        let mut tera = Tera::default();
        tera.add_raw_template("enum", ENUM_TEMPLATE).expect("Failed to add enum template");
        tera.add_raw_template("union", UNION_TEMPLATE).expect("Failed to add union template");
        tera.add_raw_template("parcelable", PARCELABLE_TEMPLATE).expect("Failed to add parcelable template");
        tera.add_raw_template("interface", INTERFACE_TEMPLATE).expect("Failed to add interface template");
        tera
    };
}

#[derive(Serialize, Deserialize, Debug)]
struct FnMembers {
    identifier: String,
    args: String,
    args_async: String,
    return_type: String,
    write_funcs: Vec<String>,
    func_call_params: String,
    transaction_decls: Vec<String>,
    transaction_write: Vec<String>,
    transaction_params: String,
    transaction_has_return: bool,
    oneway: bool,
    read_onto_params: Vec<String>,
}

fn make_fn_member(method: &parser::MethodDecl) -> Result<FnMembers, Box<dyn Error>> {
    let mut func_call_params = String::new();
    let mut args = "&self".to_string();
    let mut args_async = "&'a self".to_string();
    let mut write_funcs = Vec::new();
    let mut transaction_decls = Vec::new();
    let mut transaction_write = Vec::new();
    let mut transaction_params = String::new();
    let mut read_onto_params = Vec::new();
    // let is_nullable = parser::check_annotation_list(&method.annotation_list, parser::AnnotationType::IsNullable).0;

    method.arg_list.iter().for_each(|arg| {
        let generator = arg.to_generator();

        let type_decl_for_func = generator.type_decl_for_func();

        let arg_str = format!(", {}: {}", generator.identifier, type_decl_for_func);

        args += &arg_str;
        args_async += &arg_str.replace("&", "&'a ");
        // args_async += &if type_decl_for_func.starts_with('&') {
        //     format!(", {}: &'a {}", generator.identifier, &type_decl_for_func[1..])
        // } else {
        //     format!(", {}: {}", generator.identifier, type_decl_for_func)
        // };
        func_call_params += &format!("{}, ", generator.identifier);

        if !matches!(arg.direction, Direction::Out) {
            let param = if type_decl_for_func.starts_with('&') {
                generator.identifier.to_owned()
            } else {
                format!("&{}", generator.identifier)
            };
            write_funcs.push(format!("data.write({})?;", param));
        } else if generator.is_variable_array() {
            if generator.is_nullable {
                write_funcs.push(format!("data.write_slice_size({}.as_deref())?;", generator.identifier));
            } else {
                write_funcs.push(format!("data.write_slice_size(Some({}))?;", generator.identifier));
            }
        }

        transaction_decls.push(format!("let {};", generator.transaction_decl("_reader")));
        if matches!(arg.direction, Direction::Out) && generator.is_variable_array() {
            if generator.is_nullable {
                transaction_decls.push(format!("_reader.resize_nullable_out_vec(&mut {})?;", generator.identifier));
            } else {
                transaction_decls.push(format!("_reader.resize_out_vec(&mut {})?;", generator.identifier));
            }
        }

        if matches!(arg.direction, Direction::Out | Direction::Inout) {
            transaction_write.push(generator.identifier.to_owned());
            read_onto_params.push(generator.identifier.to_owned());
        }
        transaction_params += &format!("{}, ", generator.func_call_param());
    });

    let func_call_params = if func_call_params.chars().count() > 2 {
        func_call_params.chars().take(func_call_params.chars().count() - 2).collect::<String>()
    } else {
        func_call_params
    };

    let transaction_params = if transaction_params.chars().count() > 2 {
        transaction_params.chars().take(transaction_params.chars().count() - 2).collect::<String>()
    } else {
        transaction_params
    };

    let generator = if parser::check_annotation_list(&method.annotation_list, parser::AnnotationType::IsNullable).0 {
        method.r#type.to_generator().nullable()
    } else {
        method.r#type.to_generator()
    };

    let return_type = generator.type_declaration(false);
    let transaction_has_return = return_type != "()";

    Ok(FnMembers{
        // identifier: method.identifier.to_case(Case::Snake),
        identifier: method.identifier.to_owned(),
        args, args_async, return_type, write_funcs, func_call_params,
        transaction_decls, transaction_write, transaction_params, transaction_has_return,
        oneway: method.oneway,
        read_onto_params,
    })
}

fn gen_interface(arg_decl: &parser::InterfaceDecl, indent: usize, enabled_async: bool) -> Result<String, Box<dyn Error>> {
    let mut is_empty = false;
    let mut decl = arg_decl.clone();

    if parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::JavaOnly).0 {
        is_empty = true;
        // return Ok(String::new())
    }

    decl.pre_process();

    let mut const_members = Vec::new();
    let mut fn_members = Vec::new();

    if !is_empty {
        for constant in decl.constant_list.iter() {
            let generator = constant.r#type.to_generator();
            const_members.push((constant.const_identifier(),
                generator.const_type_decl(), generator.init_value(constant.const_expr.as_ref(), true)));
        }

        for method in decl.method_list.iter() {
            fn_members.push(make_fn_member(method)?);
        }
    }

    let enabled_async = if enabled_async || cfg!(feature = "async") { true } else { false };

    let nested = &gen_declations(&decl.members, indent + 1, enabled_async)?;

    let namespace = parser::get_descriptor_from_annotation_list(&decl.annotation_list)
        .unwrap_or_else(|| decl.namespace.to_string(Namespace::AIDL));

    let mut context = tera::Context::new();

    context.insert("mod", &decl.name);
    context.insert("name", &decl.name);
    context.insert("namespace", &namespace);
    context.insert("const_members", &const_members);
    context.insert("fn_members", &fn_members);
    context.insert("bn_name", &format!("Bn{}", &decl.name[1..]));
    context.insert("bp_name", &format!("Bp{}", &decl.name[1..]));
    context.insert("oneway", &decl.oneway);
    context.insert("nested", &nested.trim());
    context.insert("enabled_async", &enabled_async);

    let rendered = TEMPLATES.render("interface", &context).expect("Failed to render interface template");

    Ok(add_indent(indent, rendered.trim()))
}

fn gen_parcelable(arg_decl: &parser::ParcelableDecl, indent: usize, enabled_async: bool) -> Result<String, Box<dyn Error>> {
    let mut is_empty = false;
    let mut decl = arg_decl.clone();

    if parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::JavaOnly).0 {
        println!("Parcelable {} is only used for Java.", decl.name);
        is_empty = true;
        // return Ok(String::new())
    }
    if !decl.cpp_header.is_empty() {
        println!("cpp_header {} for Parcelable {} is not supported.", decl.cpp_header, decl.name);
        is_empty = true;
        // return Ok(String::new())
    }

    decl.pre_process();

    let mut constant_members = Vec::new();
    let mut members = Vec::new();
    let mut declations = Vec::new();

    if !is_empty {
        // Parse struct variables only.
        for decl in &decl.members {
            if let Some(var) = decl.is_variable() {
                let generator = var.r#type.to_generator();

                if var.constant {
                    constant_members.push((var.const_identifier(),
                        generator.const_type_decl(), generator.init_value(var.const_expr.as_ref(), true)));
                } else {
                    members.push(
                        (
                            var.identifier(),
                            generator.type_declaration(true),
                            generator.init_value(var.const_expr.as_ref(), false)
                        )
                    )
                }
            } else {
                declations.push(decl.clone());
            }
        }
    }

    let nested = &gen_declations(&declations, indent+1, enabled_async)?;
    let namespace = parser::get_descriptor_from_annotation_list(&decl.annotation_list)
        .unwrap_or_else(|| decl.namespace.to_string(Namespace::AIDL));

    let mut context = tera::Context::new();

    context.insert("mod", &decl.name);
    context.insert("name", &decl.name);
    context.insert("derive", &parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::RustDerive).1);
    context.insert("namespace", &namespace);
    context.insert("members", &members);
    context.insert("const_members", &constant_members);
    context.insert("nested", &nested.trim());

    let rendered = TEMPLATES.render("parcelable", &context).expect("Failed to render parcelable template");

    Ok(add_indent(indent, rendered.trim()))
}

fn gen_enum(decl: &parser::EnumDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    if parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::JavaOnly).0 {
        return Ok(String::new())
    }

    let generator = &parser::get_backing_type(&decl.annotation_list);

    let mut members = Vec::new();
    let mut enum_val: i64 = 0;
    for enumerator in &decl.enumerator_list {
        if let Some(const_expr) = &enumerator.const_expr {
            enum_val = const_expr.calculate().to_i64();
        }
        members.push((enumerator.identifier.to_owned(), enum_val));
        enum_val += 1;
    }

    let mut context = tera::Context::new();

    context.insert("mod", &decl.name);
    context.insert("enum_name", &decl.name);
    context.insert("enum_type", &generator.clone().direction(&Direction::None).type_declaration(true));
    context.insert("enum_len", &decl.enumerator_list.len());
    context.insert("members", &members);

    let rendered = TEMPLATES.render("enum", &context).expect("Failed to render enum template");

    Ok(add_indent(indent, rendered.trim()))
}

fn gen_union(decl: &parser::UnionDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    if parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::JavaOnly).0 {
        return Ok(String::new())
    }

    let mut constant_members = Vec::new();
    let mut members = Vec::new();

    for member in &decl.members {
        if let parser::Declaration::Variable(var) = member {
            let generator = var.r#type.to_generator();
            if var.constant {
                constant_members.push((var.const_identifier(),
                    generator.const_type_decl(), generator.init_value(var.const_expr.as_ref(), true)));
            } else {
                members.push((var.union_identifier(), generator.type_declaration(true), var.identifier(), generator.default_value()));
            }
        } else {
            unreachable!();
        }
    }

    let namespace = parser::get_descriptor_from_annotation_list(&decl.annotation_list)
        .unwrap_or_else(|| decl.namespace.to_string(Namespace::AIDL));

    let mut context = tera::Context::new();

    context.insert("mod", &decl.name);
    context.insert("union_name", &decl.name);
    context.insert("derive", &parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::RustDerive).1);
    context.insert("namespace", &namespace);
    context.insert("members", &members);
    context.insert("const_members", &constant_members);

    let rendered = TEMPLATES.render("union", &context).expect("Failed to render union template");

    Ok(add_indent(indent, rendered.trim()))
}


pub fn gen_declations(decls: &Vec<parser::Declaration>, indent: usize, enabled_async: bool) -> Result<String, Box<dyn Error>> {
    let mut content = String::new();

    for decl in decls {
        match decl {
            parser::Declaration::Interface(decl) => {
                let _ns = parser::NamespaceGuard::new(&decl.namespace);
                content += &gen_interface(decl, indent, enabled_async)?;
            }

            parser::Declaration::Parcelable(decl) => {
                let _ns = parser::NamespaceGuard::new(&decl.namespace);
                content += &gen_parcelable(decl, indent, enabled_async)?;
            }

            parser::Declaration::Variable(_decl) => {
                unreachable!("Unexpected Declaration::Variable : {:?}", _decl);
            }

            parser::Declaration::Enum(decl) => {
                let _ns = parser::NamespaceGuard::new(&decl.namespace);
                content += &gen_enum(decl, indent)?;
            }

            parser::Declaration::Union(decl) => {
                let _ns = parser::NamespaceGuard::new(&decl.namespace);
                content += &gen_union(decl, indent)?;
            }
        }
    }

    Ok(content)
}

pub fn gen_document(document: &parser::Document, enabled_async: bool) -> Result<(String, String), Box<dyn Error>> {
    parser::set_current_document(document);

    let mut content = String::new();

    content += &gen_declations(&document.decls, 0, enabled_async)?;

    Ok((document.package.clone().unwrap_or_default(), content))
}
