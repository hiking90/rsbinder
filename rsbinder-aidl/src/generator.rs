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
        {{enum_name}} : [{{enum_type}}; {{enum_len}}] {
    {%- for member in members %}
            {{ member.0 }} = {{ member.1 }},
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
    pub enum {{union_name}} {
    {%- for member in members %}
        {{ member.0 }}({{ member.1 }}),
    {%- endfor %}
    }
    {%- for member in const_members %}
    pub const {{ member.0 }}: {{ member.1 }} = {{ member.2 }};
    {%- endfor %}
    impl Default for {{union_name}} {
        fn default() -> Self {
    {%- if members|length > 0 %}
            Self::{{members[0][0]}}({{members[0][3]}})
    {%- endif %}
        }
    }
    impl rsbinder::Parcelable for {{union_name}} {
        fn write_to_parcel(&self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match self {
    {%- set counter = 0 %}
    {%- for member in members %}
                Self::{{member.0}}(v) => {
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
                    *self = Self::{{member.0}}(value);
                    Ok(())
                }
    {%- set_global counter = counter + 1 %}
    {%- endfor %}
                _ => Err(rsbinder::StatusCode::BadValue),
            }
        }
    }
    rsbinder::impl_serialize_for_parcelable!({{union_name}});
    rsbinder::impl_deserialize_for_parcelable!({{union_name}});
    impl rsbinder::ParcelableMetadata for {{union_name}} {
        fn descriptor() -> &'static str { "{{ namespace }}" }
    }
    rsbinder::declare_binder_enum! {
        Tag : [i32; {{ members|length }}] {
    {%- set counter = 0 %}
    {%- for member in members %}
            {{ member.2 }} = {{ counter }},
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
    pub const {{ member.0 }}: {{ member.1 }} = {{ member.2 }};
    {%- endfor %}
    #[derive(Debug)]
    {%- if derive|length > 0 %}
    #[derive({{ derive }})]
    {%- endif %}
    pub struct {{name}} {
    {%- for member in members %}
        pub {{ member.0 }}: {{ member.1 }},
    {%- endfor %}
    }
    impl Default for {{ name }} {
        fn default() -> Self {
            Self {
            {%- for member in members %}
                {{ member.0 }}: {{ member.2 }},
            {%- endfor %}
            }
        }
    }
    impl rsbinder::Parcelable for {{name}} {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                {%- for member in members %}
                _sub_parcel.write(&self.{{ member.0 }})?;
                {%- endfor %}
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                {%- for member in members %}
                self.{{ member.0 }} = _sub_parcel.read()?;
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
    pub const {{ member.0 }}: {{ member.1 }} = {{ member.2 }};
    {%- endfor %}
    pub trait {{name}}: rsbinder::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "{{ namespace }}" }
        {%- for member in fn_members %}
        fn {{ member.identifier }}({{ member.args }}) -> rsbinder::status::Result<{{ member.return_type }}>;
        {%- endfor %}
        fn getDefaultImpl() -> {{ name }}DefaultRef where Self: Sized {
            DEFAULT_IMPL.lock().unwrap().clone()
        }
        fn setDefaultImpl(d: {{ name }}DefaultRef) -> {{ name }}DefaultRef where Self: Sized {
            std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
        }
    }
    pub trait {{ name }}Default: Send + Sync {
        {%- for member in fn_members %}
        fn {{ member.identifier }}({{ member.args }}) -> rsbinder::status::Result<{{ member.return_type }}> {
            Err(rsbinder::StatusCode::UnknownTransaction.into())
        }
        {%- endfor %}
    }
    pub(crate) mod transactions {
        {%- set counter = 0 %}
        {%- for member in fn_members %}
        pub(crate) const {{ member.identifier }}: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + {{ counter }};
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
            native: {{ bn_name }}(on_transact),
            proxy: {{ bp_name }},
        }
    }
    impl {{ bp_name }} {
        {%- for member in fn_members %}
        fn build_parcel_{{ member.identifier }}({{ member.args }}) -> rsbinder::Result<rsbinder::Parcel> {
            {%- if member.write_params|length > 0 %}
            let mut data = self.binder.as_proxy().unwrap().prepare_transact(true)?;
            {%- for arg in member.write_params %}
            data.write({{ arg }})?;
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
                  return _aidl_default_impl.{{ member.identifier }}({{ member.func_call_params }});
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
        fn {{ member.identifier }}({{ member.args }}) -> rsbinder::status::Result<{{ member.return_type }}> {
            let _aidl_data = self.build_parcel_{{ member.identifier }}({{ member.func_call_params }})?;
            let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::{{ member.identifier }}, &_aidl_data, {% if oneway or member.oneway %}rsbinder::FLAG_ONEWAY | {% endif %}rsbinder::FLAG_PRIVATE_VENDOR);
            {%- if member.func_call_params|length > 0 %}
            self.read_response_{{ member.identifier }}({{ member.func_call_params }}, _aidl_reply)
            {%- else %}
            self.read_response_{{ member.identifier }}(_aidl_reply)
            {%- endif %}
        }
        {%- endfor %}
    }
    impl {{ name }} for rsbinder::Binder<{{ bn_name }}> {
        {%- for member in fn_members %}
        fn {{ member.identifier }}({{ member.args }}) -> rsbinder::status::Result<{{ member.return_type }}> {
            self.0.{{ member.identifier }}({{ member.func_call_params }})
        }
        {%- endfor %}
    }
    fn on_transact(
        _service: &dyn {{ name }}, _code: rsbinder::TransactionCode, _reader: &mut rsbinder::Parcel, _reply: &mut rsbinder::Parcel, _descriptor: &str) -> rsbinder::Result<()> {
        match _code {
        {%- for member in fn_members %}
            transactions::{{ member.identifier }} => {
            {%- for decl in member.transaction_decls %}
                let {{ decl }};
            {%- endfor %}
                let _aidl_return = _service.{{ member.identifier }}({{ member.transaction_params }});
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
    return_type: String,
    write_params: Vec<String>,
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
    let mut write_params = Vec::new();
    let mut transaction_decls = Vec::new();
    let mut transaction_write = Vec::new();
    let mut transaction_params = String::new();
    let mut read_onto_params = Vec::new();
    // let is_nullable = parser::check_annotation_list(&method.annotation_list, parser::AnnotationType::IsNullable).0;

    method.arg_list.iter().for_each(|arg| {
        let generator = arg.to_generator();

        let type_decl_for_func = generator.type_decl_for_func();
        args += &format!(", {}: {}", generator.identifier, type_decl_for_func);
        func_call_params += &format!("{}, ", generator.identifier);

        if !matches!(arg.direction, Direction::Out) {
            if type_decl_for_func.starts_with('&') {
                write_params.push(generator.identifier.to_owned());
            } else {
                write_params.push(format!("&{}", generator.identifier));
            }
        }

        transaction_decls.push(generator.transaction_decl("_reader"));

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
        args, return_type, write_params, func_call_params,
        transaction_decls, transaction_write, transaction_params, transaction_has_return,
        oneway: method.oneway,
        read_onto_params,
    })
}

fn gen_interface(arg_decl: &parser::InterfaceDecl, indent: usize) -> Result<String, Box<dyn Error>> {
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

    let nested = &gen_declations(&decl.members, indent + 1)?;

    let mut context = tera::Context::new();

    context.insert("mod", &decl.name);
    context.insert("name", &decl.name);
    context.insert("namespace", &decl.namespace.to_string(Namespace::AIDL));
    context.insert("const_members", &const_members);
    context.insert("fn_members", &fn_members);
    context.insert("bn_name", &format!("Bn{}", &decl.name[1..]));
    context.insert("bp_name", &format!("Bp{}", &decl.name[1..]));
    context.insert("oneway", &decl.oneway);
    context.insert("nested", &nested.trim());

    let rendered = TEMPLATES.render("interface", &context).expect("Failed to render interface template");

    Ok(add_indent(indent, rendered.trim()))
}

fn gen_parcelable(arg_decl: &parser::ParcelableDecl, indent: usize) -> Result<String, Box<dyn Error>> {
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

    let nested = &gen_declations(&declations, indent+1)?;

    let mut context = tera::Context::new();

    context.insert("mod", &decl.name);
    context.insert("name", &decl.name);
    context.insert("derive", &parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::RustDerive).1);
    context.insert("namespace", &decl.namespace.to_string(Namespace::AIDL));
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
            todo!();
        }
    }

    let mut context = tera::Context::new();

    context.insert("mod", &decl.name);
    context.insert("union_name", &decl.name);
    context.insert("derive", &parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::RustDerive).1);
    context.insert("namespace", &decl.namespace.to_string(Namespace::AIDL));
    context.insert("members", &members);
    context.insert("const_members", &constant_members);

    let rendered = TEMPLATES.render("union", &context).expect("Failed to render union template");

    Ok(add_indent(indent, rendered.trim()))
}


pub fn gen_declations(decls: &Vec<parser::Declaration>, indent: usize) -> Result<String, Box<dyn Error>> {
    let mut content = String::new();

    for decl in decls {
        match decl {
            parser::Declaration::Interface(decl) => {
                let _ns = parser::NamespaceGuard::new(&decl.namespace);
                content += &gen_interface(decl, indent)?;
            }

            parser::Declaration::Parcelable(decl) => {
                let _ns = parser::NamespaceGuard::new(&decl.namespace);
                content += &gen_parcelable(decl, indent)?;
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

pub fn gen_document(document: &parser::Document) -> Result<(String, String), Box<dyn Error>> {
    parser::set_current_document(document);

    let mut content = String::new();

    content += &gen_declations(&document.decls, 0)?;

    Ok((document.package.clone().unwrap_or_default(), content))
}
