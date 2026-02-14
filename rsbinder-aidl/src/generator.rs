// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use std::error::Error;

use tera::Tera;

use crate::const_expr::{ConstExpr, InitParam, ValueType};
use crate::parser::Direction;
use crate::{add_indent, parser, Namespace};

const ENUM_TEMPLATE: &str = r##"
pub mod {{mod}} {
    #![allow(non_upper_case_globals, non_snake_case)]
    {{crate}}::declare_binder_enum! {
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
    impl {{crate}}::Parcelable for r#{{union_name}} {
        fn write_to_parcel(&self, parcel: &mut {{crate}}::Parcel) -> {{crate}}::Result<()> {
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
        fn read_from_parcel(&mut self, parcel: &mut {{crate}}::Parcel) -> {{crate}}::Result<()> {
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
                _ => Err({{crate}}::StatusCode::BadValue),
            }
        }
    }
    {{crate}}::impl_serialize_for_parcelable!(r#{{union_name}});
    {{crate}}::impl_deserialize_for_parcelable!(r#{{union_name}});
    impl {{crate}}::ParcelableMetadata for r#{{union_name}} {
        fn descriptor() -> &'static str { "{{ namespace }}" }
        {%- if is_vintf %}
        fn stability(&self) -> {{crate}}::Stability { {{crate}}::Stability::Vintf }
        {%- endif %}
    }
    {{crate}}::declare_binder_enum! {
        Tag : [i32; {{ members|length }}] {
    {%- set counter = 0 %}
    {%- for member in members %}
            r#{{ member.2 }} = {{ counter }},
    {%- set_global counter = counter + 1 %}
    {%- endfor %}
        }
    }
    {%- if nested|length>0 %}
    {{nested}}
    {%- endif %}
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
    impl {{crate}}::Parcelable for {{name}} {
        fn write_to_parcel(&self, _parcel: &mut {{crate}}::Parcel) -> {{crate}}::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                {%- for member in members %}
                _sub_parcel.write(&self.r#{{ member.0 }})?;
                {%- endfor %}
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut {{crate}}::Parcel) -> {{crate}}::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                {%- for member in members %}
                self.r#{{ member.0 }} = _sub_parcel.read()?;
                {%- endfor %}
                Ok(())
            })
        }
    }
    {{crate}}::impl_serialize_for_parcelable!({{name}});
    {{crate}}::impl_deserialize_for_parcelable!({{name}});
    impl {{crate}}::ParcelableMetadata for {{name}} {
        fn descriptor() -> &'static str { "{{namespace}}" }
        {%- if is_vintf %}
        fn stability(&self) -> {{crate}}::Stability { {{crate}}::Stability::Vintf }
        {%- endif %}
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
    pub trait {{name}}: {{crate}}::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "{{ namespace }}" }
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}({{ member.args }}) -> {{crate}}::status::Result<{{ member.return_type }}>;
        {%- endfor %}
        fn getDefaultImpl() -> Option<{{ name }}DefaultRef> where Self: Sized {
            DEFAULT_IMPL.get().cloned()
        }
        fn setDefaultImpl(d: {{ name }}DefaultRef) -> {{ name }}DefaultRef where Self: Sized {
            DEFAULT_IMPL.get_or_init(|| d).clone()
        }
    }
    {%- if enabled_async %}
    pub trait {{name}}Async<P>: {{crate}}::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "{{ namespace }}" }
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}<'a>({{ member.args_async }}) -> {{crate}}::BoxFuture<'a, {{crate}}::status::Result<{{ member.return_type }}>>;
        {%- endfor %}
    }
    #[::async_trait::async_trait]
    pub trait {{name}}AsyncService: {{crate}}::Interface + Send {
        fn descriptor() -> &'static str where Self: Sized { "{{ namespace }}" }
        {%- for member in fn_members %}
        async fn r#{{ member.identifier }}({{ member.args }}) -> {{crate}}::status::Result<{{ member.return_type }}>;
        {%- endfor %}
    }
    impl {{bn_name}}
    {
        pub fn new_async_binder<T, R>(inner: T, rt: R) -> {{crate}}::Strong<dyn {{name}}>
        where
            T: {{name}}AsyncService + Sync + Send + 'static,
            R: {{crate}}::BinderAsyncRuntime + Send + Sync + 'static,
        {
            struct Wrapper<T, R> {
                _inner: T,
                _rt: R,
            }
            impl<T, R> {{crate}}::Interface for Wrapper<T, R> where T: {{crate}}::Interface, R: Send + Sync {
                fn as_binder(&self) -> {{crate}}::SIBinder { self._inner.as_binder() }
                fn dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> {{crate}}::Result<()> { self._inner.dump(_writer, _args) }
            }
            impl<T, R> {{bn_name}}Adapter for Wrapper<T, R>
            where
                T: {{name}}AsyncService + Sync + Send + 'static,
                R: {{crate}}::BinderAsyncRuntime + Send + Sync + 'static,
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
                R: {{crate}}::BinderAsyncRuntime + Send + Sync + 'static,
            {
                {%- for member in fn_members %}
                fn r#{{ member.identifier }}({{ member.args }}) -> {{crate}}::status::Result<{{ member.return_type }}> {
                    self._rt.block_on(self._inner.r#{{ member.identifier }}({{ member.func_call_params }}))
                }
                {%- endfor %}
            }
            let wrapped = Wrapper { _inner: inner, _rt: rt };
            {%- if is_vintf %}
            let binder = {{crate}}::native::Binder::new_with_stability({{bn_name}}(Box::new(wrapped)), {{crate}}::Stability::Vintf);
            {%- else %}
            let binder = {{crate}}::native::Binder::new_with_stability({{bn_name}}(Box::new(wrapped)), {{crate}}::Stability::default());
            {%- endif %}
            {{crate}}::Strong::new(Box::new(binder))
        }
    }
    {%- endif %}
    pub trait {{ name }}Default: Send + Sync {
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}({{ member.args }}) -> {{crate}}::status::Result<{{ member.return_type }}> {
            Err({{crate}}::StatusCode::UnknownTransaction.into())
        }
        {%- endfor %}
    }
    pub(crate) mod transactions {
        {%- set counter = 0 %}
        {%- for member in fn_members %}
        {%- if member.has_explicit_code %}
        pub(crate) const r#{{ member.identifier }}: {{crate}}::TransactionCode = {{crate}}::FIRST_CALL_TRANSACTION + {{ member.transaction_code }};
        {%- else %}
        pub(crate) const r#{{ member.identifier }}: {{crate}}::TransactionCode = {{crate}}::FIRST_CALL_TRANSACTION + {{ counter }};
        {%- set_global counter = counter + 1 %}
        {%- endif %}
        {%- endfor %}
    }
    pub type {{ name }}DefaultRef = std::sync::Arc<dyn {{ name }}Default>;
    static DEFAULT_IMPL: std::sync::OnceLock<{{ name }}DefaultRef> = std::sync::OnceLock::new();
    {{crate}}::declare_binder_interface! {
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
        fn build_parcel_{{ member.identifier }}({{ member.args }}) -> {{crate}}::Result<{{crate}}::Parcel> {
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
        fn read_response_{{ member.identifier }}({{ member.args }}, _aidl_reply: {{crate}}::Result<Option<{{crate}}::Parcel>>) -> {{crate}}::status::Result<{{ member.return_type }}> {
            {%- if oneway or member.oneway %}
            Ok(())
            {%- else %}
            if let Err({{crate}}::StatusCode::UnknownTransaction) = _aidl_reply {
                if let Some(_aidl_default_impl) = <Self as {{name}}>::getDefaultImpl() {
                  return _aidl_default_impl.r#{{ member.identifier }}({{ member.func_call_params }});
                }
            }
            let mut _aidl_reply = _aidl_reply?.ok_or({{crate}}::StatusCode::UnexpectedNull)?;
            let _status = _aidl_reply.read::<{{crate}}::Status>()?;
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
        fn r#{{ member.identifier }}({{ member.args }}) -> {{crate}}::status::Result<{{ member.return_type }}> {
            let _aidl_data = self.build_parcel_{{ member.identifier }}({{ member.func_call_params }})?;
            let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::r#{{ member.identifier }}, &_aidl_data, {% if oneway or member.oneway %}{{crate}}::FLAG_ONEWAY | {% endif %}{{crate}}::FLAG_CLEAR_BUF);
            {%- if member.func_call_params|length > 0 %}
            self.read_response_{{ member.identifier }}({{ member.func_call_params }}, _aidl_reply)
            {%- else %}
            self.read_response_{{ member.identifier }}(_aidl_reply)
            {%- endif %}
        }
        {%- endfor %}
    }
    {%- if enabled_async %}
    impl<P: {{crate}}::BinderAsyncPool> {{name}}Async<P> for {{ bp_name }} {
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}<'a>({{ member.args_async }}) -> {{crate}}::BoxFuture<'a, {{crate}}::status::Result<{{ member.return_type }}>> {
            let _aidl_data = match self.build_parcel_{{ member.identifier }}({{ member.func_call_params }}) {
                Ok(_aidl_data) => _aidl_data,
                Err(err) => return Box::pin(std::future::ready(Err(err.into()))),
            };
            let binder = self.binder.clone();
            P::spawn(
                move || binder.as_proxy().unwrap().submit_transact(transactions::r#{{ member.identifier }}, &_aidl_data, {{crate}}::FLAG_CLEAR_BUF | {{crate}}::FLAG_PRIVATE_LOCAL),
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
    impl<P: {{crate}}::BinderAsyncPool> {{name}}Async<P> for {{crate}}::Binder<{{bn_name}}>
    {
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}<'a>({{ member.args_async }}) -> {{crate}}::BoxFuture<'a, {{crate}}::status::Result<{{ member.return_type }}>> {
            self.0.as_async().r#{{ member.identifier }}({{ member.func_call_params }})
        }
        {%- endfor %}
    }
    {%- endif %}
    impl {{ name }} for {{crate}}::Binder<{{ bn_name }}> {
        {%- for member in fn_members %}
        fn r#{{ member.identifier }}({{ member.args }}) -> {{crate}}::status::Result<{{ member.return_type }}> {
            {%- if enabled_async %}
            self.0.as_sync().r#{{ member.identifier }}({{ member.func_call_params }})
            {%- else %}
            self.0.r#{{ member.identifier }}({{ member.func_call_params }})
            {%- endif %}
        }
        {%- endfor %}
    }
    fn on_transact(
        _service: &dyn {{ name }}, _code: {{crate}}::TransactionCode, _reader: &mut {{crate}}::Parcel, _reply: &mut {{crate}}::Parcel) -> {{crate}}::Result<()> {
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
                        _reply.write(&{{crate}}::Status::from({{crate}}::StatusCode::Ok))?;
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
            _ => Err({{crate}}::StatusCode::UnknownTransaction),
        }
    }
    {%- if nested|length>0 %}
    {{nested}}
    {%- endif %}
}
"#;

fn template() -> &'static tera::Tera {
    static TEMPLATES: std::sync::OnceLock<tera::Tera> = std::sync::OnceLock::new();

    TEMPLATES.get_or_init(|| {
        let mut tera = Tera::default();
        tera.add_raw_template("enum", ENUM_TEMPLATE)
            .expect("Failed to add enum template");
        tera.add_raw_template("union", UNION_TEMPLATE)
            .expect("Failed to add union template");
        tera.add_raw_template("parcelable", PARCELABLE_TEMPLATE)
            .expect("Failed to add parcelable template");
        tera.add_raw_template("interface", INTERFACE_TEMPLATE)
            .expect("Failed to add interface template");
        tera
    })
}

// lazy_static! {
//     pub static ref TEMPLATES: Tera = {
//         let mut tera = Tera::default();
//         tera.add_raw_template("enum", ENUM_TEMPLATE)
//             .expect("Failed to add enum template");
//         tera.add_raw_template("union", UNION_TEMPLATE)
//             .expect("Failed to add union template");
//         tera.add_raw_template("parcelable", PARCELABLE_TEMPLATE)
//             .expect("Failed to add parcelable template");
//         tera.add_raw_template("interface", INTERFACE_TEMPLATE)
//             .expect("Failed to add interface template");
//         tera
//     };
// }

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
    transaction_code: u32,
    has_explicit_code: bool,
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
        args_async += &arg_str.replace('&', "&'a ");
        func_call_params += &format!("{}, ", generator.identifier);

        if !matches!(arg.direction, Direction::Out) {
            let param = if type_decl_for_func.starts_with('&') {
                generator.identifier.to_owned()
            } else {
                format!("&{}", generator.identifier)
            };
            write_funcs.push(format!("data.write({param})?;"));
        } else if generator.is_variable_array() {
            if generator.is_nullable {
                write_funcs.push(format!(
                    "data.write_slice_size({}.as_deref())?;",
                    generator.identifier
                ));
            } else {
                write_funcs.push(format!(
                    "data.write_slice_size(Some({}))?;",
                    generator.identifier
                ));
            }
        }

        transaction_decls.push(format!("let {};", generator.transaction_decl("_reader")));
        if matches!(arg.direction, Direction::Out) && generator.is_variable_array() {
            if generator.is_nullable {
                transaction_decls.push(format!(
                    "_reader.resize_nullable_out_vec(&mut {})?;",
                    generator.identifier
                ));
            } else {
                transaction_decls.push(format!(
                    "_reader.resize_out_vec(&mut {})?;",
                    generator.identifier
                ));
            }
        }

        if matches!(arg.direction, Direction::Out | Direction::Inout) {
            transaction_write.push(generator.identifier.to_owned());
            read_onto_params.push(generator.identifier.to_owned());
        }
        transaction_params += &format!("{}, ", generator.func_call_param());
    });

    let func_call_params = if func_call_params.chars().count() > 2 {
        func_call_params
            .chars()
            .take(func_call_params.chars().count() - 2)
            .collect::<String>()
    } else {
        func_call_params
    };

    let transaction_params = if transaction_params.chars().count() > 2 {
        transaction_params
            .chars()
            .take(transaction_params.chars().count() - 2)
            .collect::<String>()
    } else {
        transaction_params
    };

    let generator = if parser::check_annotation_list(
        &method.annotation_list,
        parser::AnnotationType::IsNullable,
    )
    .0
    {
        method.r#type.to_generator().nullable()
    } else {
        method.r#type.to_generator()
    };

    let return_type = generator.type_declaration(false);
    let transaction_has_return = return_type != "()";

    Ok(FnMembers {
        // identifier: method.identifier.to_case(Case::Snake),
        identifier: method.identifier.to_owned(),
        args,
        args_async,
        return_type,
        write_funcs,
        func_call_params,
        transaction_decls,
        transaction_write,
        transaction_params,
        transaction_has_return,
        oneway: method.oneway,
        read_onto_params,
        transaction_code: method.intvalue.unwrap_or(0) as u32,
        has_explicit_code: method.intvalue.is_some(),
    })
}

pub struct Generator {
    enabled_async: bool,
    is_crate: bool,
}

impl Generator {
    pub fn new(enabled_async: bool, is_crate: bool) -> Self {
        Self {
            enabled_async,
            is_crate,
        }
    }

    fn get_crate_name(&self) -> &str {
        if self.is_crate {
            "crate"
        } else {
            "rsbinder"
        }
    }

    fn new_context(&self) -> tera::Context {
        let mut context = tera::Context::new();

        context.insert("crate", self.get_crate_name());

        context
    }

    /// Pre-register all enum member symbols from a document into the symbol table.
    /// This ensures enum symbols are available before any code generation begins,
    /// preventing incorrect resolution when multiple enums share the same member names.
    pub fn pre_register_enums(document: &parser::Document) {
        parser::set_current_document(document);
        Self::pre_register_enum_decls(&document.decls);
    }

    fn pre_register_enum_decls(decls: &[parser::Declaration]) {
        for decl in decls {
            match decl {
                parser::Declaration::Enum(enum_decl) => {
                    let _ns = parser::NamespaceGuard::new(&enum_decl.namespace);
                    Self::register_enum_members(enum_decl);
                }
                parser::Declaration::Parcelable(d) => Self::pre_register_enum_decls(&d.members),
                parser::Declaration::Interface(d) => Self::pre_register_enum_decls(&d.members),
                parser::Declaration::Union(d) => Self::pre_register_enum_decls(&d.members),
                _ => {}
            }
        }
    }

    pub fn document(
        &self,
        document: &parser::Document,
    ) -> Result<(String, String), Box<dyn Error>> {
        parser::set_current_document(document);

        let mut content = String::new();

        content += &self.declarations(&document.decls, 0)?;

        Ok((document.package.clone().unwrap_or_default(), content))
    }

    pub fn declarations(
        &self,
        decls: &Vec<parser::Declaration>,
        indent: usize,
    ) -> Result<String, Box<dyn Error>> {
        let mut content = String::new();

        for decl in decls {
            match decl {
                parser::Declaration::Interface(decl) => {
                    let _ns = parser::NamespaceGuard::new(&decl.namespace);
                    content += &self.decl_interface(decl, indent)?;
                }

                parser::Declaration::Parcelable(decl) => {
                    let _ns = parser::NamespaceGuard::new(&decl.namespace);
                    content += &self.decl_parcelable(decl, indent)?;
                }

                parser::Declaration::Variable(_decl) => {
                    unreachable!("Unexpected Declaration::Variable : {:?}", _decl);
                }

                parser::Declaration::Enum(decl) => {
                    let _ns = parser::NamespaceGuard::new(&decl.namespace);
                    content += &self.decl_enum(decl, indent)?;
                }

                parser::Declaration::Union(decl) => {
                    let _ns = parser::NamespaceGuard::new(&decl.namespace);
                    content += &self.decl_union(decl, indent)?;
                }
            }
        }

        Ok(content)
    }

    fn decl_interface(
        &self,
        arg_decl: &parser::InterfaceDecl,
        indent: usize,
    ) -> Result<String, Box<dyn Error>> {
        let mut decl = arg_decl.clone();

        let is_empty =
            parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::JavaOnly)
                .0;
        let is_vintf = parser::check_annotation_list(
            &decl.annotation_list,
            parser::AnnotationType::VintfStability,
        )
        .0;

        decl.pre_process();

        let mut const_members = Vec::new();
        let mut fn_members = Vec::new();

        if !is_empty {
            // First pass: register all interface constants for resolution
            for constant in decl.constant_list.iter() {
                if let Some(const_expr) = &constant.const_expr {
                    parser::register_symbol(
                        &constant.identifier,
                        const_expr.clone(),
                        parser::SymbolType::InterfaceConstant,
                        Some(&decl.name),
                    );
                }
            }

            // Second pass: process constants with resolved values
            for constant in decl.constant_list.iter() {
                let generator = constant.r#type.to_generator();
                const_members.push((
                    constant.const_identifier(),
                    generator.const_type_decl(),
                    generator.init_value(
                        constant.const_expr.as_ref(),
                        InitParam::builder().with_const(true),
                    ),
                ));
            }

            // === Transaction code validation (before fn_members loop) ===
            {
                let explicit_count = decl.method_list.iter()
                    .filter(|m| m.intvalue.is_some())
                    .count();

                // AOSP rule: all-or-nothing
                if explicit_count > 0 && explicit_count < decl.method_list.len() {
                    return Err(format!(
                        "Interface {}: either all methods must have explicitly assigned \
                         transaction IDs or none of them should",
                        decl.name
                    ).into());
                }

                if explicit_count > 0 {
                    // Detect duplicate transaction codes
                    let mut seen = std::collections::HashMap::new();
                    for method in decl.method_list.iter() {
                        if let Some(code) = method.intvalue {
                            if code < 0 {
                                return Err(format!(
                                    "Interface {}: method '{}' has negative transaction code {}",
                                    decl.name, method.identifier, code
                                ).into());
                            }
                            if code > u32::MAX as i64 {
                                return Err(format!(
                                    "Interface {}: method '{}' has transaction code {} exceeding u32 range",
                                    decl.name, method.identifier, code
                                ).into());
                            }
                            if let Some(prev_name) = seen.insert(code, &method.identifier) {
                                return Err(format!(
                                    "Interface {}: methods '{}' and '{}' have the same \
                                     transaction code {}",
                                    decl.name, prev_name, method.identifier, code
                                ).into());
                            }
                        }
                    }
                }
            }

            for method in decl.method_list.iter() {
                fn_members.push(make_fn_member(method)?);
            }
        }

        let enabled_async = self.enabled_async;

        let nested = &self.declarations(&decl.members, indent + 1)?;

        let namespace = parser::get_descriptor_from_annotation_list(&decl.annotation_list)
            .unwrap_or_else(|| decl.namespace.to_string(Namespace::AIDL));

        let mut context = self.new_context();

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
        context.insert("is_vintf", &is_vintf);

        let rendered = template()
            .render("interface", &context)
            .expect("Failed to render interface template");

        Ok(add_indent(indent, rendered.trim()))
    }

    fn decl_parcelable(
        &self,
        arg_decl: &parser::ParcelableDecl,
        indent: usize,
    ) -> Result<String, Box<dyn Error>> {
        let mut is_empty = false;
        let mut decl = arg_decl.clone();

        let is_vintf = parser::check_annotation_list(
            &decl.annotation_list,
            parser::AnnotationType::VintfStability,
        )
        .0;

        if parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::JavaOnly).0
        {
            println!("Parcelable {} is only used for Java.", decl.name);
            is_empty = true;
            // return Ok(String::new())
        }
        if !decl.cpp_header.is_empty() {
            println!(
                "cpp_header {} for Parcelable {} is not supported.",
                decl.cpp_header, decl.name
            );
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
                    println!("Parcelable variable: {var:?}");
                    let generator = var.r#type.to_generator();

                    if var.constant {
                        constant_members.push((
                            var.const_identifier(),
                            generator.const_type_decl(),
                            generator.init_value(
                                var.const_expr.as_ref(),
                                InitParam::builder().with_const(true),
                            ),
                        ));
                    } else {
                        let init_value = match generator.value_type {
                            ValueType::Holder => Some(ConstExpr::new(ValueType::Holder)),
                            _ => var.const_expr.clone(),
                        };

                        members.push((
                            var.identifier(),
                            generator.type_declaration(true),
                            generator.init_value(
                                init_value.as_ref(),
                                InitParam::builder()
                                    .with_const(false)
                                    .with_vintf(is_vintf)
                                    .with_crate_name(self.get_crate_name()),
                            ),
                        ))
                    }
                } else {
                    declations.push(decl.clone());
                }
            }
        }

        let nested = &self.declarations(&declations, indent + 1)?;
        let namespace = parser::get_descriptor_from_annotation_list(&decl.annotation_list)
            .unwrap_or_else(|| decl.namespace.to_string(Namespace::AIDL));

        let mut context = self.new_context();

        context.insert("mod", &decl.name);
        context.insert("name", &decl.name);
        context.insert(
            "derive",
            &parser::check_annotation_list(
                &decl.annotation_list,
                parser::AnnotationType::RustDerive,
            )
            .1,
        );
        context.insert("namespace", &namespace);
        context.insert("members", &members);
        context.insert("const_members", &constant_members);
        context.insert("nested", &nested.trim());
        context.insert("is_vintf", &is_vintf);

        let rendered = template()
            .render("parcelable", &context)
            .expect("Failed to render parcelable template");

        Ok(add_indent(indent, rendered.trim()))
    }

    fn register_enum_members(decl: &parser::EnumDecl) {
        if parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::JavaOnly).0
        {
            return;
        }

        let mut enum_val: i64 = 0;

        for enumerator in &decl.enumerator_list {
            let member_name = &enumerator.identifier;

            if let Some(const_expr) = &enumerator.const_expr {
                // Try to compute the value, but handle cases where it might reference other enum members
                let calculated = const_expr.calculate();
                let computed_val = match calculated.value {
                    crate::const_expr::ValueType::Name(_) => {
                        // This is an unresolved reference, use current enum_val as fallback
                        enum_val
                    }
                    _ => calculated.to_i64(),
                };

                parser::register_symbol(
                    member_name,
                    crate::const_expr::ConstExpr::new(crate::const_expr::ValueType::Reference {
                        enum_name: decl.name.clone(),
                        member_name: member_name.to_string(),
                        value: computed_val,
                    }),
                    parser::SymbolType::EnumMember,
                    Some(&decl.name),
                );

                enum_val = computed_val;
            } else {
                parser::register_symbol(
                    member_name,
                    crate::const_expr::ConstExpr::new(crate::const_expr::ValueType::Reference {
                        enum_name: decl.name.clone(),
                        member_name: member_name.to_string(),
                        value: enum_val,
                    }),
                    parser::SymbolType::EnumMember,
                    Some(&decl.name),
                );
            }

            enum_val += 1;
        }
    }

    fn decl_enum(&self, decl: &parser::EnumDecl, indent: usize) -> Result<String, Box<dyn Error>> {
        if parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::JavaOnly).0
        {
            return Ok(String::new());
        }

        let generator = &parser::get_backing_type(&decl.annotation_list);

        let mut members = Vec::new();

        // First pass: register all enum members with their names for resolution
        Self::register_enum_members(decl);

        // Second pass: resolve all values now that all members are registered
        let mut enum_val: i64 = 0;
        for enumerator in &decl.enumerator_list {
            if let Some(const_expr) = &enumerator.const_expr {
                enum_val = const_expr.calculate().to_i64();
            }
            members.push((enumerator.identifier.to_owned(), enum_val));
            enum_val += 1;
        }

        let mut context = self.new_context();

        context.insert("mod", &decl.name);
        context.insert("enum_name", &decl.name);
        context.insert(
            "enum_type",
            &generator
                .clone()
                .direction(&Direction::None)
                .type_declaration(true),
        );
        context.insert("enum_len", &decl.enumerator_list.len());
        context.insert("members", &members);

        let rendered = template()
            .render("enum", &context)
            .expect("Failed to render enum template");

        Ok(add_indent(indent, rendered.trim()))
    }

    fn decl_union(
        &self,
        decl: &parser::UnionDecl,
        indent: usize,
    ) -> Result<String, Box<dyn Error>> {
        if parser::check_annotation_list(&decl.annotation_list, parser::AnnotationType::JavaOnly).0
        {
            return Ok(String::new());
        }

        let is_vintf = parser::check_annotation_list(
            &decl.annotation_list,
            parser::AnnotationType::VintfStability,
        )
        .0;

        let mut constant_members = Vec::new();
        let mut members = Vec::new();
        let mut declarations = Vec::new();

        for member in &decl.members {
            if let parser::Declaration::Variable(var) = member {
                let generator = var.r#type.to_generator();
                if var.constant {
                    constant_members.push((
                        var.const_identifier(),
                        generator.const_type_decl(),
                        generator.init_value(
                            var.const_expr.as_ref(),
                            InitParam::builder().with_const(true),
                        ),
                    ));
                } else {
                    members.push((
                        var.union_identifier(),
                        generator.type_declaration(true),
                        var.identifier(),
                        generator.default_value(),
                    ));
                }
            } else {
                declarations.push(member.clone());
            }
        }

        let nested = &self.declarations(&declarations, indent + 1)?;
        let namespace = parser::get_descriptor_from_annotation_list(&decl.annotation_list)
            .unwrap_or_else(|| decl.namespace.to_string(Namespace::AIDL));

        let mut context = self.new_context();

        context.insert("mod", &decl.name);
        context.insert("union_name", &decl.name);
        context.insert(
            "derive",
            &parser::check_annotation_list(
                &decl.annotation_list,
                parser::AnnotationType::RustDerive,
            )
            .1,
        );
        context.insert("namespace", &namespace);
        context.insert("members", &members);
        context.insert("const_members", &constant_members);
        context.insert("nested", &nested.trim());
        context.insert("is_vintf", &is_vintf);

        let rendered = template()
            .render("union", &context)
            .expect("Failed to render union template");

        Ok(add_indent(indent, rendered.trim()))
    }
}
