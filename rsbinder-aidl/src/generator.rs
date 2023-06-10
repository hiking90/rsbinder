use std::error::Error;

use convert_case::{Case, Casing};
use crate::{parser, indent_space};

pub fn attribute(indent: usize) -> String {
    let mut content = String::new();
    let indent = indent_space(indent);

    content += &(indent.clone() + "use std::sync::Arc;\n");
    content += &(indent + "use rsbinder;\n");

    return content;
}

fn gen_method(method: &parser::MethodDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    let mut args = "&self".to_string();

    method.arg_list.iter().for_each(|arg| {
        args += &format!(", {}", arg.to_string());
    });

    let return_type = if parser::is_nullable(&method.annotation_list) == true {
        format!("Option<{}>", method.r#type.to_string(false))
    } else {
        method.r#type.to_string(false)
    };

    let api = format!("{}fn {}({args}) -> rsbinder::Result<{return_type}>;\n",
        indent_space(indent),
        method.identifier.to_case(Case::Snake));

    Ok(api)
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

fn gen_interface(decl: &parser::InterfaceDecl, indent: usize) -> Result<String, Box<dyn Error>> {
    let mut content = String::new();

    let mut decl = decl.clone();
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

    content += &(indent_space(indent) + &format!("pub trait {}: rsbinder::Interface + Send {{\n", decl.name));
    for method in decl.method_list.iter() {
        content += &gen_method(method, indent + 1)?;
    }
    content += &(indent_space(indent) + "}\n\n");
    content += &(indent_space(indent) + "mod transactions {\n");

    let mut idx = 0;
    for method in decl.method_list.iter() {
        content += &format!("{}const {}: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + {idx};\n",
            indent_space(indent + 1),
            method.identifier.to_case(Case::UpperSnake));
        idx  += 1;
    }

    content += &(indent_space(indent) + "}\n");

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