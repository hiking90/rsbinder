use std::collections::HashMap;
use std::error::Error;

use convert_case::{Case, Casing};
use crate::{parser, indent_space, const_expr};

pub fn attribute(indent: usize) -> String {
    let mut content = String::new();
    let indent = indent_space(indent);

    content += &(indent.clone() + "use std::sync::Arc;\n");
    // content += &(indent + "use rsbinder;\n");

    return content;
}

// decl.namespace has "aidl::android::os" and it must be converted from "aidl::android::os" to "android.os".
fn to_namespace(namespace: &str, name: &str) -> String {
    let namespace = namespace.trim_start_matches(&(crate::DEFAULT_NAMESPACE.to_owned() + "::"));
    let namespace = namespace.replace("::", ".");

    format!("{namespace}.{name}")
}

fn gen_method(method: &parser::MethodDecl, indent: usize) -> Result<(String, String, String), Box<dyn Error>> {
    let mut build_params = String::new();
    let mut read_params = String::new();
    let mut args = "&self".to_string();
    let mut build_parcel = Vec::new();

    method.arg_list.iter().for_each(|arg| {
        let arg_str = arg.to_string();
        args += &format!(", {}", arg_str.1);
        build_params += &format!("{}.clone(), ", arg_str.0);
        read_params += &format!("{}, ", arg_str.0);

        // It generates body of build_parcel_functions.
        let (ref_str, func_str) = if arg.r#type.is_clonable() == true {
            if arg.r#type.is_declared() == true {
                ("", ".as_ref()")
            } else {
                ("&", "")
            }
        } else {
            ("", "")
        };
        build_parcel.push(format!("data.write({}{}{})?;", ref_str, arg_str.0, func_str));
    });

    if build_parcel.len() > 0 {
        build_parcel.insert(0, "let mut data = self.handle.prepare_transact(true)?;".to_owned());
    } else {
        build_parcel.push("let data = self.handle.prepare_transact(true)?;".to_owned());
    }

    let return_type = if parser::is_nullable(&method.annotation_list) == true {
        format!("Option<{}>", method.r#type.to_string(false))
    } else {
        method.r#type.to_string(false)
    };

    let method_identifier = method.identifier.to_case(Case::Snake);
    let indent = indent_space(indent);

    let api = format!("{indent}fn {}({args}) -> rsbinder::Result<{return_type}>;\n",
        method_identifier);

    // Generate build_parcel_{}
    let mut proxy_struct_method = format!("{indent}fn build_parcel_{}({args}) -> rsbinder::Result<rsbinder::Parcel> {{\n",
        method_identifier);
    build_parcel.iter().for_each(|line| {
        proxy_struct_method += &format!("{indent}{}{}\n", indent_space(1), line);
    });
    proxy_struct_method += &format!("{indent}{}Ok(data)\n{indent}}}\n", indent_space(1));

    // Generate read_response_{}
    proxy_struct_method += &format!("{indent}fn read_response_{}({args}, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<{return_type}> {{\n",
        method_identifier);

    if method.r#type.to_string(false) == "()" {
        proxy_struct_method += &format!("{indent}{}Ok(())\n", indent_space(1));
    } else {
        proxy_struct_method += &format!("{indent}{}let mut _aidl_reply = _aidl_reply.unwrap();\n", indent_space(1));
        proxy_struct_method += &format!("{indent}{}let _status = _aidl_reply.read::<rsbinder::Status>()?;\n", indent_space(1));
        proxy_struct_method += &format!("{indent}{}let _aidl_return: {return_type} = _aidl_reply.read()?;\n", indent_space(1));
        proxy_struct_method += &format!("{indent}{}Ok(_aidl_return)\n", indent_space(1));
    }

    // proxy_struct_method += &format!("{indent}{}todo!()\n", indent_space(1));
    proxy_struct_method += &format!("{indent}}}\n");

    let mut proxy_interface_method = format!("{indent}fn {}({args}) -> rsbinder::Result<{return_type}> {{\n",
        method_identifier);

    // let _aidl_data = self.build_parcel_get_service(_arg_name)?;
    // let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::GET_SERVICE, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR);
    // self.read_response_get_service(_arg_name, _aidl_reply)

    proxy_interface_method += &format!("{indent}{}let _aidl_data = self.build_parcel_{}({})?;\n",
        indent_space(1), method_identifier, build_params);
    proxy_interface_method += &format!("{indent}{}let _aidl_reply = self.handle.submit_transact(transactions::{}, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;\n",
        indent_space(1), method.identifier.to_case(Case::UpperSnake));
    if read_params.len() > 0 {
        proxy_interface_method += &format!("{indent}{}self.read_response_{}({}_aidl_reply)\n",
            indent_space(1), method_identifier, read_params);
    } else {
        proxy_interface_method += &format!("{indent}{}self.read_response_{}(_aidl_reply)\n",
            indent_space(1), method_identifier);
    }

    // proxy_interface_method += &format!("{indent}{}todo!()\n", indent_space(1));
    proxy_interface_method += &format!("{indent}}}\n");

    Ok((api, proxy_struct_method, proxy_interface_method))
}

fn begin_mod(name: &str, indent: usize) -> String {
    let mut content = String::new();
    content += &(indent_space(indent) + &format!("mod {} {{\n", name.to_case(Case::Snake)));
    content += &attribute(indent + 1);
    content
}

fn end_mod(indent: usize) -> String {
    indent_space(indent) + "}\n"
}

fn gen_declare_binder_interface(decl: &parser::InterfaceDecl, indent: usize) -> (String, String, String) {
    let indent = indent_space(indent);
    let mut content = format!("{indent}rsbinder::declare_binder_interface! {{\n");

    let native_name = format!("Bn{}", &decl.name[1..]);
    let proxy_name = format!("Bp{}", &decl.name[1..]);

    let namespace = to_namespace(&decl.namespace, &decl.name);

    content += &format!("{indent}{}{}[\"{}\"] {{\n", indent_space(1), decl.name, namespace);
    content += &format!("{indent}{}native: {native_name}(on_transact),\n", indent_space(2));
    content += &format!("{indent}{}proxy: {proxy_name},\n", indent_space(2));
    content += &format!("{indent}{}}}\n", indent_space(1));
    content += &format!("{indent}}}\n");

    (content, native_name, proxy_name)
}

fn gen_native(decl: &parser::InterfaceDecl, indent: usize) -> String {
    let mut content = format!("{}fn on_transact(\n", indent_space(indent));

    content += &format!("{}_service: &dyn {}, _code: rsbinder::TransactionCode,) -> rsbinder::Result<()> {{\n",
        indent_space(indent + 1),
        decl.name);
    content += &format!("{}Ok(())\n", indent_space(indent + 1));
    content += &format!("{}}}\n", indent_space(indent));
    content
}

fn gen_proxy(decl: &parser::InterfaceDecl, name: &str, indent: usize) -> (String, String) {
    let impl_struct = format!("{}impl {} {{\n", indent_space(indent), name);
    let impl_interface = format!("{}impl {} for {} {{\n", indent_space(indent), decl.name, name);

    (impl_struct, impl_interface)
}

fn gen_interface(arg_decl: &parser::InterfaceDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    let mut content = String::new();

    let mut decl = arg_decl.clone();
    decl.post_process()?;

    content += &begin_mod(&decl.name, indent);
    let indent = indent + 1;

    for constant in decl.constant_list.iter() {
        content += &(indent_space(indent) + &constant.to_string());
    }

    if decl.constant_list.len() > 0 {
        content += "\n";
    }

    for _member in decl.members.iter() {
        todo!();
    }

    let declare_binder_interface = gen_declare_binder_interface(arg_decl, indent);
    let generated_native = gen_native(arg_decl, indent);
    let mut generated_proxy = gen_proxy(arg_decl, &declare_binder_interface.2, indent);

    content += &(indent_space(indent) + &format!("pub trait {}: rsbinder::Interface + Send {{\n", decl.name));
    for method in decl.method_list.iter() {
        let res = gen_method(method, indent + 1)?;

        content += &res.0;
        generated_proxy.0 += &res.1;
        generated_proxy.1 += &res.2;
    }
    content += &(indent_space(indent) + "}\n\n");
    content += &(indent_space(indent) + "pub(crate) mod transactions {\n");

    let mut idx = 0;
    for method in decl.method_list.iter() {
        content += &format!("{}pub(crate) const {}: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + {idx};\n",
            indent_space(indent + 1),
            method.identifier.to_case(Case::UpperSnake));
        idx  += 1;
    }

    content += &(indent_space(indent) + "}\n");

    content += &declare_binder_interface.0;

    content += &generated_proxy.0;
    content += &(indent_space(indent) + "}\n\n");

    content += &generated_proxy.1;
    content += &(indent_space(indent) + "}\n\n");

    content += &generated_native;

    content += &end_mod(indent - 1);

    Ok(content)
}

fn gen_parcelable(decl: &parser::ParcelableDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    let mut decl = decl.clone();
    decl.post_process()?;

    let mut content = String::new();

    content += &begin_mod(&decl.name, indent);
    let indent = indent + 1;

    content += &(indent_space(indent) + "#[derive(Debug)]\n");
    content += &(indent_space(indent) + &format!("pub struct {} {{\n", decl.name));

    // Parse struct variables only.
    for decl in &decl.members {
        if let Some(decl) = decl.is_variable() {
            content += &gen_variable(decl, indent + 1)?;
        }
    }

    content += &(indent_space(indent) + "}\n");

    content += &(indent_space(indent) + &format!("impl Default for {} {{\n", decl.name));
    content += &(indent_space(indent + 1) + "fn default() -> Self {\n");
    content += &(indent_space(indent + 2) + "Self {\n");

    // Parse struct variables only.
    for decl in &decl.members {
        if let Some(decl) = decl.is_variable() {
            content += &(indent_space(indent+3) + &decl.to_default());
        }
    }
    content += &(indent_space(indent + 2) + "}\n");
    content += &(indent_space(indent + 1) + "}\n");
    content += &(indent_space(indent) + "}\n");

    // Generate Parcelable

    content += &format!("{}impl rsbinder::Parcelable for {} {{\n", indent_space(indent), decl.name);

    content += &(indent_space(indent + 1) + "fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {\n");
    for decl in &decl.members {
        if let Some(decl) = decl.is_variable() {
            content += &format!("{}_parcel.write(&self.{})?;\n", indent_space(indent+2), decl.identifier());
        }
    }
    content += &(indent_space(indent + 2) + "Ok(())\n");
    content += &(indent_space(indent + 1) + "}\n");

    content += &(indent_space(indent + 1) + "fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {\n");
    for decl in &decl.members {
        if let Some(decl) = decl.is_variable() {
            content += &format!("{}self.{} = _parcel.read()?;\n", indent_space(indent+2), decl.identifier());
        }
    }
    content += &(indent_space(indent + 2) + "Ok(())\n");
    content += &(indent_space(indent + 1) + "}\n");

    content += &(indent_space(indent) + "}\n");

    content += &format!("{}rsbinder::impl_serialize_for_parcelable!({});\n", indent_space(indent), decl.name);
    content += &format!("{}rsbinder::impl_deserialize_for_parcelable!({});\n", indent_space(indent), decl.name);

    content += &format!("{}impl rsbinder::ParcelableMetadata for {} {{\n", indent_space(indent), decl.name);

    let namespace = to_namespace(&decl.namespace, &decl.name);

    content += &format!("{}fn get_descriptor() -> &'static str {{ \"{}\" }}\n", indent_space(indent+1), namespace);
    content += &(indent_space(indent) + "}\n");

    // impl rsbinder::ParcelableMetadata for ConnectionInfo {
    //   fn get_descriptor() -> &'static str { "android.os.ConnectionInfo" }
    // }

    content += &gen_declations(&decl.members, indent+1)?;

    content += &end_mod(indent - 1);

    Ok(content)
}

fn gen_variable(decl: &parser::VariableDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    let mut content = String::new();

    content += &(indent_space(indent) + &decl.to_string());

    Ok(content)
}

fn gen_enum(decl: &parser::EnumDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    let indent = indent_space(indent);
    let mut content = String::new();

    content += &format!("{indent}declare_binder_enum! {{\n");
    content += &format!("{indent}{}{} : [{}; {}] {{\n", indent_space(1),
        decl.name, parser::get_backing_type(&decl.annotation_list), decl.enumerator_list.len());

    let mut enum_val: i64 = 0;
    for enumerator in &decl.enumerator_list {
        if let Some(const_expr) = &enumerator.const_expr {
            if let const_expr::ConstExpr::Expression(expr) = const_expr.calculate(&mut HashMap::new())? {
                enum_val = expr.to_i64()?;
            }
        }
        content += &format!("{indent}{}{} = {},\n", indent_space(2),
            enumerator.identifier, enum_val);
        enum_val += 1;
    }

    content += &format!("{indent}{}}}\n", indent_space(1));
    content += &(indent + "}\n");

    Ok(content)
}

fn gen_union(decl: &parser::UnionDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    let indent = indent_space(indent);
    let mut content = String::new();
    let mut constant_members = Vec::new();

    content += &format!("{indent}#[derive(Debug, Clone, PartialEq)]\n");
    content += &format!("{indent}pub enum {} {{\n", decl.name);

    let mut first_var: Option<&parser::VariableDecl> = None;

    for member in &decl.members {
        if let parser::Declaration::Variable(var) = member {
            if var.constant == true {
                constant_members.push(var);
            } else {
                if let None = first_var {
                    first_var = Some(var);
                }
                content += &format!("{indent}{}{}\n", indent_space(1), var.to_enum_member());
            }
        } else {
            todo!()
        }
    }

    content += &format!("{indent}}}\n");

    for var in constant_members {
        content += &format!("{indent}{}\n", var.to_string());
    }

    content += &format!("{indent}impl Default for {} {{\n", decl.name);
    content += &format!("{indent}{}fn default() -> Self {{\n", indent_space(1));
    content += &match first_var {
        Some(var) => format!("{indent}{}Self::{}(Default::default())\n", indent_space(2), var.member_identifier()),
        None => format!("{indent}{}Self {{}}\n", indent_space(2)),
    };
    content += &format!("{indent}{}}}\n", indent_space(1));
    content += &format!("{indent}}}\n");

    content += &format!("{indent}impl rsbinder::Parcelable for {} {{\n", decl.name);

    content += &format!("{indent}{}fn write_to_parcel(&self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {{\n", indent_space(1));
    content += &format!("{indent}{}match self {{\n", indent_space(2));
    let mut tag: i32 = 0;
    for member in &decl.members {
        if let parser::Declaration::Variable(var) = member {
            if var.constant == false {
                content += &format!("{indent}{}Self::{}(v) => {{\n", indent_space(3), var.member_identifier());
                content += &format!("{indent}{}parcel.write(&{}i32)?;\n", indent_space(4), tag);
                content += &format!("{indent}{}parcel.write(v)\n", indent_space(4));
                content += &format!("{indent}{}}}\n", indent_space(3));
                tag += 1;
            }
        } else {
            todo!()
        }
    }

    content += &format!("{indent}{}}}\n", indent_space(2));
    content += &format!("{indent}{}}}\n", indent_space(1));

    content += &format!("{indent}{}fn read_from_parcel(&mut self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {{\n", indent_space(1));
    content += &format!("{indent}{}let tag: i32 = parcel.read()?;\n", indent_space(2));
    content += &format!("{indent}{}match tag {{\n", indent_space(2));

    tag = 0;
    for member in &decl.members {
        if let parser::Declaration::Variable(var) = member {
            if var.constant == false {
                content += &format!("{indent}{}{} => {{\n", indent_space(3), tag);
                content += &format!("{indent}{}let value: {} = parcel.read()?;\n", indent_space(4), var.member_type());
                content += &format!("{indent}{}*self = Self::{}(value);\n", indent_space(4), var.member_identifier());
                content += &format!("{indent}{}Ok(())\n", indent_space(4));
                content += &format!("{indent}{}}}\n", indent_space(3));
                tag += 1;
            }
        } else {
            todo!()
        }
    }

    content += &format!("{indent}{}_ => Err(rsbinder::StatusCode::BadValue.into()),\n", indent_space(3));

    content += &format!("{indent}{}}}\n", indent_space(2));
    content += &format!("{indent}{}}}\n", indent_space(1));

    content += &format!("{indent}}}\n");

    content += &format!("{indent}rsbinder::impl_serialize_for_parcelable!({});\n", decl.name);
    content += &format!("{indent}rsbinder::impl_deserialize_for_parcelable!({});\n", decl.name);

    content += &format!("{indent}impl rsbinder::ParcelableMetadata for {} {{\n", decl.name);
    content += &format!("{indent}{}fn get_descriptor() -> &'static str {{ \"{}\" }}\n", indent_space(1), to_namespace(&decl.namespace, &decl.name));
    content += &format!("{indent}}}\n");

    content += &format!("{indent}pub mod tag {{\n");
    content += &format!("{indent}{}rsbinder::declare_binder_enum! {{\n", indent_space(1));
    content += &format!("{indent}{}Tag : [i32; {}] {{\n", indent_space(2), tag);

    tag = 0;
    for member in &decl.members {
        if let parser::Declaration::Variable(var) = member {
            if var.constant == false {
                content += &format!("{indent}{}{} = {},\n", indent_space(3), var.identifier.to_case(Case::UpperSnake), tag);
                tag += 1;
            }
        } else {
            todo!()
        }
    }

    content += &format!("{indent}{}}}\n", indent_space(2));
    content += &format!("{indent}{}}}\n", indent_space(1));
    content += &format!("{indent}}}\n");

    Ok(content)
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

    // content += "const SOME_REPLY_EXPECTED: &str = \"reply parcel must be valid.\"\n";

    content += &gen_declations(&document.decls, 0)?;

    Ok((document.package.clone().unwrap_or_default(), content))
}