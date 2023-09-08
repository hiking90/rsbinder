
use std::error::Error;

use tera::Tera;
use convert_case::{Case, Casing};

use crate::{parser, add_indent};

const ENUM_TEMPLATE: &str = r##"
pub use {{mod}}::*;
mod {{mod}} {
    declare_binder_enum! {
        {{enum_name}} : [{{enum_type}}; {{enum_len}}] {
    {%- for member in members %}
            {{ member.0 }} = {{ member.1 }},
    {%- endfor %}
        }
    }
}
"##;

const UNION_TEMPLATE: &str = r##"
pub use {{mod}}::*;
mod {{mod}} {
    #[derive(Debug, Clone, PartialEq)]
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
            Self::{{members[0][0]}}(Default::default())
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
                _ => Err(rsbinder::StatusCode::BadValue.into()),
            }
        }
    }
    rsbinder::impl_serialize_for_parcelable!({{union_name}});
    rsbinder::impl_deserialize_for_parcelable!({{union_name}});
    impl rsbinder::ParcelableMetadata for {{union_name}} {
        fn get_descriptor() -> &'static str { "{{ namespace }}" }
    }
    pub mod tag {
        rsbinder::declare_binder_enum! {
            Tag : [i32; {{ members|length }}] {
    {%- set counter = 0 %}
    {%- for member in members %}
                {{ member.0|upper }} = {{ counter }},
    {%- set_global counter = counter + 1 %}
    {%- endfor %}
            }
        }
    }
}
"##;

const PARCELABLE_TEMPLATE: &str = r##"
pub use {{mod}}::*;
mod {{mod}} {
    {%- for member in const_members %}
    pub const {{ member.0 }}: {{ member.1 }} = {{ member.2 }};
    {%- endfor %}
    #[derive(Debug, Default)]
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
            {%- for member in members %}
            _parcel.write(&self.{{ member.0 }})?;
            {%- endfor %}
            Ok(())
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            {%- for member in members %}
            self.{{ member.0 }} = _parcel.read()?;
            {%- endfor %}
            Ok(())
        }
    }
    rsbinder::impl_serialize_for_parcelable!({{name}});
    rsbinder::impl_deserialize_for_parcelable!({{name}});
    impl rsbinder::ParcelableMetadata for {{name}} {
        fn get_descriptor() -> &'static str { "{{namespace}}" }
    }
}
"##;

const INTERFACE_TEMPLATE: &str = r##"
pub use {{mod}}::*;
mod {{mod}} {
    {%- for member in const_members %}
    pub const {{ member.0 }}: {{ member.1 }} = {{ member.2 }};
    {%- endfor %}
    pub trait {{name}}: rsbinder::Interface + Send {
        {%- for member in fn_members %}
        fn {{ member.0 }}({{ member.1 }}) -> rsbinder::Result<{{ member.2 }}>;
        {%- endfor %}
    }
    pub(crate) mod transactions {
        {%- set counter = 0 %}
        {%- for member in fn_members %}
        pub(crate) const {{ member.0|upper }}: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + {{ counter }};
        {%- set_global counter = counter + 1 %}
        {%- endfor %}
    }
    rsbinder::declare_binder_interface! {
        {{ name }}["{{ namespace }}"] {
            native: {{ bn_name }}(on_transact),
            proxy: {{ bp_name }},
        }
    }
    impl {{ bp_name }} {
        {%- for member in fn_members %}
        fn build_parcel_{{ member.0 }}({{ member.1 }}) -> rsbinder::Result<rsbinder::Parcel> {
            {%- if member.3|length > 0 %}
            let mut data = self.handle.prepare_transact(true)?;
            {%- for arg in member.3 %}
            data.write({{ arg }})?;
            {%- endfor %}
            {%- else %}
            let data = self.handle.prepare_transact(true)?;
            {%- endif %}
            Ok(data)
        }
        fn read_response_{{ member.0 }}({{ member.1 }}, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<{{ member.2 }}> {
            {%- if oneway or member.6 %}
            Ok(())
            {%- else %}
            {%- if member.2 != "()" %}
            let mut _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            let _aidl_return: {{ member.2 }} = _aidl_reply.read()?;
            Ok(_aidl_return)
            {%- else %}
            let _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            Ok(())
            {%- endif %}
            {%- endif %}
        }
        {%- endfor %}
    }
    impl {{ name }} for {{ bp_name }} {
        {%- for member in fn_members %}
        fn {{ member.0 }}({{ member.1 }}) -> rsbinder::Result<{{ member.2 }}> {
            let _aidl_data = self.build_parcel_{{ member.0 }}({{ member.4 }})?;
            let _aidl_reply = self.handle.submit_transact(transactions::{{ member.0|upper }}, &_aidl_data, {% if oneway or member.6 %}rsbinder::FLAG_ONEWAY | {% endif %}rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_{{ member.0 }}({{ member.5 }}_aidl_reply)
        }
        {%- endfor %}
    }
    fn on_transact(
        _service: &dyn {{ name }}, _code: rsbinder::TransactionCode,) -> rsbinder::Result<()> {
        Ok(())
    }
}
"##;

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

// decl.namespace has "aidl::android::os" and it must be converted from "aidl::android::os" to "android.os".
fn to_namespace(namespace: &str, name: &str) -> String {
    let namespace = namespace.trim_start_matches(&(crate::DEFAULT_NAMESPACE.to_owned() + "::"));
    let namespace = namespace.replace("::", ".");

    format!("{namespace}.{name}")
}

fn make_fn_member(method: &parser::MethodDecl) -> Result<(String, String, String, Vec<String>, String, String, bool), Box<dyn Error>> {
    let mut build_params = String::new();
    let mut read_params = String::new();
    let mut args = "&self".to_string();
    let mut write_params = Vec::new();

    method.arg_list.iter().for_each(|arg| {
        let arg_str = arg.to_string();
        args += &format!(", {}", arg_str.1);

        let type_cast = arg.r#type.type_cast();

        if type_cast.is_declared {
            build_params += &format!("{}.clone(), ", arg_str.0);
            write_params.push(format!("{}.as_ref()", arg_str.0))
        } else if type_cast.is_primitive {
            build_params += &format!("{}, ", arg_str.0);
            write_params.push(format!("&{}", arg_str.0))
        } else {
            if type_cast.is_string {
                build_params += &format!("{}, ", arg_str.0);
                write_params.push(format!("{}", arg_str.0))
            } else {
                build_params += &format!("{}.clone(), ", arg_str.0);
                write_params.push(format!("&{}", arg_str.0))
            }
        }

        // build_params += &format!("{}.clone(), ", arg_str.0);
        read_params += &format!("{}, ", arg_str.0);

        // write_params.push(type_cast.as_ref(&arg_str.0));
    });

    let return_type = method.r#type.type_cast().return_type(parser::is_nullable(&method.annotation_list));

    Ok((method.identifier.to_case(Case::Snake),
        args, return_type, write_params, build_params, read_params, method.oneway))
}


fn gen_interface(arg_decl: &parser::InterfaceDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    let mut decl = arg_decl.clone();
    decl.pre_process();

    let mut const_members = Vec::new();
    for constant in decl.constant_list.iter() {
        let type_cast = constant.r#type.type_cast();
        const_members.push((constant.const_identifier(),
            type_cast.const_type(), type_cast.init_type(constant.const_expr.as_ref(), true)));
    }

    let mut fn_members = Vec::new();
    for method in decl.method_list.iter() {
        fn_members.push(make_fn_member(method)?);
    }

    let mut context = tera::Context::new();

    context.insert("mod", &decl.name.to_case(Case::Snake));
    context.insert("name", &decl.name);
    context.insert("namespace", &to_namespace(&decl.namespace, &decl.name));
    context.insert("const_members", &const_members);
    context.insert("fn_members", &fn_members);
    context.insert("bn_name", &format!("Bn{}", &decl.name[1..]));
    context.insert("bp_name", &format!("Bp{}", &decl.name[1..]));
    context.insert("oneway", &decl.oneway);
    // let native_name = format!("Bn{}", &decl.name[1..]);
    // let proxy_name = format!("Bp{}", &decl.name[1..]);

    let rendered = TEMPLATES.render("interface", &context).expect("Failed to render interface template");

    Ok(add_indent(indent, &rendered.trim()))
}

fn gen_parcelable(arg_decl: &parser::ParcelableDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    let mut decl = arg_decl.clone();
    decl.pre_process();

    let mut constant_members = Vec::new();
    let mut members = Vec::new();
    // Parse struct variables only.
    for decl in &decl.members {
        if let Some(var) = decl.is_variable() {
            let type_cast = var.r#type.type_cast();

            if var.constant == true {
                constant_members.push((var.const_identifier(),
                    type_cast.const_type(), type_cast.init_type(var.const_expr.as_ref(), true)));
            } else {
                members.push(
                    (var.identifier(), type_cast.member_type(), type_cast.init_type(var.const_expr.as_ref(), false))
                )
            }
        }
    }

    let mut context = tera::Context::new();

    context.insert("mod", &decl.name.to_case(Case::Snake));
    context.insert("name", &decl.name);
    context.insert("namespace", &to_namespace(&decl.namespace, &decl.name));
    context.insert("members", &members);
    context.insert("const_members", &constant_members);

    let rendered = TEMPLATES.render("parcelable", &context).expect("Failed to render parcelable template");

    Ok(add_indent(indent, &rendered.trim()))
}

fn gen_enum(decl: &parser::EnumDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    let type_cast = &parser::get_backing_type(&decl.annotation_list);

    let mut members = Vec::new();
    let mut enum_val: i64 = 0;
    for enumerator in &decl.enumerator_list {
        if let Some(const_expr) = &enumerator.const_expr {
            enum_val = const_expr.calculate(None).to_i64(None);
        }
        members.push((&enumerator.identifier, enum_val));
        enum_val += 1;
    }

    let mut context = tera::Context::new();

    context.insert("mod", &decl.name.to_case(Case::Snake));
    context.insert("enum_name", &decl.name);
    context.insert("enum_type", &type_cast.type_name());
    context.insert("enum_len", &decl.enumerator_list.len());
    context.insert("members", &members);

    let rendered = TEMPLATES.render("enum", &context).expect("Failed to render enum template");

    Ok(add_indent(indent, &rendered.trim()))
}

fn gen_union(decl: &parser::UnionDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    let mut constant_members = Vec::new();
    let mut members = Vec::new();

    for member in &decl.members {
        if let parser::Declaration::Variable(var) = member {
            let type_cast = var.r#type.type_cast();
            if var.constant == true {
                constant_members.push((var.const_identifier(),
                    type_cast.const_type(), type_cast.init_type(var.const_expr.as_ref(), true)));
            } else {
                members.push((var.union_identifier(), type_cast.member_type()));
            }
        } else {
            todo!();
        }
    }

    let mut context = tera::Context::new();

    context.insert("mod", &decl.name.to_case(Case::Snake));
    context.insert("union_name", &decl.name);
    context.insert("namespace", &to_namespace(&decl.namespace, &decl.name));
    context.insert("members", &members);
    context.insert("const_members", &constant_members);

    let rendered = TEMPLATES.render("union", &context).expect("Failed to render union template");

    Ok(add_indent(indent, &rendered.trim()))
}


pub fn gen_declations(decls: &Vec<parser::Declaration>, indent: usize) -> Result<String, Box<dyn Error>> {
    let mut content = String::new();

    for decl in decls {
        match decl {
            parser::Declaration::Interface(decl) => {
                content += &gen_interface(decl, indent)?;
            }

            parser::Declaration::Parcelable(decl) => {
                content += &gen_parcelable(decl, indent)?;
            }

            parser::Declaration::Variable(_decl) => {
                unreachable!("Unexpected Declaration::Variable : {:?}", _decl);
            }

            parser::Declaration::Enum(decl) => {
                content += &gen_enum(decl, indent)?;
            }

            parser::Declaration::Union(decl) => {
                content += &gen_union(decl, indent)?;
            }
        }
    }

    Ok(content)
}

pub fn gen_document(document: &parser::Document) -> Result<(String, String), Box<dyn Error>> {
    let mut content = String::new();

    content += &gen_declations(&document.decls, 0)?;

    Ok((document.package.clone().unwrap_or_default(), content))
}