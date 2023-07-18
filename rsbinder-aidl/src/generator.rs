use std::error::Error;

use convert_case::{Case, Casing};
use crate::{parser, indent_space};

pub fn attribute(indent: usize) -> String {
    let mut content = String::new();
    let indent = indent_space(indent);

    content += &(indent.clone() + "use std::sync::Arc;\n");
    // content += &(indent + "use rsbinder;\n");

    return content;
}

fn gen_method(method: &parser::MethodDecl, indent: usize) -> Result<(String, String, String), Box<dyn Error>> {
    let mut build_params = String::new();
    let mut read_params = String::new();
    let mut args = "&self".to_string();

    method.arg_list.iter().for_each(|arg| {
        let arg = arg.to_string();
        args += &format!(", {}", arg.1);
        build_params += &format!("{}.clone(), ", arg.0);
        read_params += &format!("{}, ", arg.0);
    });

    let return_type = if parser::is_nullable(&method.annotation_list) == true {
        format!("Option<{}>", method.r#type.to_string(false))
    } else {
        method.r#type.to_string(false)
    };

    let method_identifier = method.identifier.to_case(Case::Snake);
    let indent = indent_space(indent);

    let api = format!("{indent}fn {}({args}) -> rsbinder::Result<{return_type}>;\n",
        method_identifier);

    let mut proxy_struct_method = format!("{indent}fn build_parcel_{}({args}) -> rsbinder::Result<rsbinder::Parcel> {{\n",
        method_identifier);
    proxy_struct_method += &format!("{indent}{}todo!()\n{indent}}}\n", indent_space(1));
    proxy_struct_method += &format!("{indent}fn read_response_{}({args}, _aidl_reply: rsbinder::Result<Option<rsbinder::Parcel>>) -> rsbinder::Result<{return_type}> {{\n",
        method_identifier);
    proxy_struct_method += &format!("{indent}{}todo!()\n", indent_space(1));
    proxy_struct_method += &format!("{indent}}}\n");

    let mut proxy_interface_method = format!("{indent}fn {}({args}) -> rsbinder::Result<{return_type}> {{\n",
        method_identifier);

    // let _aidl_data = self.build_parcel_get_service(_arg_name)?;
    // let _aidl_reply = self.binder.as_proxy().unwrap().submit_transact(transactions::GET_SERVICE, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR);
    // self.read_response_get_service(_arg_name, _aidl_reply)

    proxy_interface_method += &format!("{indent}{}let _aidl_data = self.build_parcel_{}({})?;\n",
        indent_space(1), method_identifier, build_params);
    proxy_interface_method += &format!("{indent}{}let _aidl_reply = self.handle.submit_transact(transactions::{}, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR);\n",
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

    content += &format!("{indent}{}{}[\"{}.{}\"] {{\n", indent_space(1), decl.name, decl.namespace, decl.name);
    content += &format!("{indent}{}native: {native_name}(on_transact),\n", indent_space(2));
    content += &format!("{indent}{}proxy: {proxy_name},\n", indent_space(2));
    content += &format!("{indent}{}}}\n", indent_space(1));
    content += &format!("{indent}}}\n");

    (content, native_name, proxy_name)
}

fn gen_native(decl: &parser::InterfaceDecl, indent: usize) -> String {
    let mut content = format!("{}fn on_transact(\n", indent_space(indent));

    content += &format!("{}service: &dyn {}, code: rsbinder::TransactionCode,) -> rsbinder::Result<()> {{\n",
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
    content += &(indent_space(indent) + "pub mod transactions {\n");

    let mut idx = 0;
    for method in decl.method_list.iter() {
        content += &format!("{}pub const {}: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + {idx};\n",
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

    content += &gen_declations(&decl.members, indent+1)?;

    content += &end_mod(indent - 1);

    Ok(content)
}

fn gen_variable(decl: &parser::VariableDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    let mut content = String::new();

    content += &(indent_space(indent) + &decl.to_string());

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

            _ => todo!(),
        }
    }

    Ok(content)
}

pub fn gen_document(document: &parser::Document) -> Result<(String, String), Box<dyn Error>> {
    let mut content = String::new();

    content += &gen_declations(&document.decls, 0)?;

    Ok((document.package.clone().unwrap_or_default(), content))
}