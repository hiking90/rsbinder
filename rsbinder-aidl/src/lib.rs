#[macro_use]
extern crate lazy_static;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::fs;

mod parser;
mod generator;
mod const_expr;
pub use parser::parse_document;
pub use generator::gen_document;

pub const DEFAULT_NAMESPACE: &str = "aidl";

pub fn indent_space(step: usize) -> String {
    let indent = "    ";
    let mut ret = String::new();

    for _ in 0..step {
        ret += indent;
    }

    ret
}

pub fn add_indent(step: usize, source: &str) -> String {
    let mut content = String::new();
    for line in source.lines() {
        if line.len() > 0 {
            content += &(indent_space(step) + line + "\n");
        } else {
            content += "\n";
        }
    }
    content
}

fn add_namespace(namespace: &str, source: &str) -> String {
    let mut content = String::new();

    content += &format!("pub mod {} {{\n", namespace);
    content += &add_indent(1, source);
    content += "}\n";

    content
}

pub struct Builder {
    sources: Vec<PathBuf>,
    dest_dir: PathBuf,
}

impl Builder {
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
            dest_dir: PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or("aidl_gen".into())),
        }
    }

    pub fn new_with_destination(dest: PathBuf) -> Self {
        Self {
            sources: Vec::new(),
            dest_dir: dest,
        }
    }

    pub fn source(mut self, source: PathBuf) -> Self {
        self.sources.push(source);
        self
    }

    fn parse_file(filename: &Path) -> Result<(String, parser::Document), Box<dyn Error>> {
        let unparsed_file = fs::read_to_string(filename.clone())?;
        let document = parser::parse_document(&unparsed_file)?;

        Ok((filename.file_stem().unwrap().to_str().unwrap().to_string(), document))

        // let package = generator::gen_document(&document)?;

        // Ok((package.0, package.1, filename.file_stem().unwrap().to_str().unwrap().to_string()))
    }

    fn traverse_source(&self, dir: &Path) -> Result<Vec<(String, parser::Document)>, Box<dyn Error>> {
        let entries = fs::read_dir(dir).map_err(|err| {
            eprintln!("traverse_source: fs::read_dir({dir:?}) failed.");
            err
        }).unwrap();
        let mut package_list = Vec::new();

        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                package_list.append(&mut self.traverse_source(&path)?);
            }
            if path.is_file() && path.extension().unwrap_or_default() == "aidl" {
                let package = Self::parse_file(&path)?;
                package_list.push(package);
            }
        }

        Ok(package_list)
    }

    fn generate_all(&self, mut package_list: Vec<(String, String, String)>) -> Result<String, Box<dyn Error>> {
        let mut content = String::new();
        let mut namespace = String::new();
        let mut mod_count:usize = 0;

        content += "#[allow(unused_imports)]\n\n";

        package_list.sort();

        for package in package_list {
            if namespace != package.0 {
                let namespace_split: Vec<&str> = namespace.split('.').collect();
                let mod_list: Vec<&str> = package.0.split('.').collect();

                let cmp_len = std::cmp::min(namespace_split.len(), mod_list.len());
                let mut start = 0;

                for i in 0..cmp_len {
                    if namespace_split[i] == mod_list[i] {
                        start += 1;
                    } else {
                        break;
                    }
                }

                for i in (start..mod_count).rev() {
                    content += &indent_space(i);
                    content += "}\n";
                }

                namespace = package.0.clone();
                mod_count = start;

                for r#mod in &mod_list[start..] {
                    content += &indent_space(mod_count);
                    content += &format!("pub mod {} {{\n", r#mod);
                    mod_count += 1;
                }

            }

            content += &add_indent(mod_count, &package.1);
        }

        for i in (0..mod_count).rev() {
            content += &indent_space(i);
            content += "}\n";
        }

        Ok(content)
    }

    pub fn generate(self) -> Result<(), Box<dyn Error>> {
        let mut document_list = Vec::new();

        for source in &self.sources {
            if source.is_file() {
                let package = Self::parse_file(&source)?;
                document_list.push(package);
            } else {
                document_list.append(&mut self.traverse_source(&source)?);
            };
        }

        let mut package_list = Vec::new();
        for document in document_list {
            let package = generator::gen_document(&document.1)?;
            package_list.push((package.0, package.1, document.0));
        }

        let content = self.generate_all(package_list)?;
        let content = add_namespace(DEFAULT_NAMESPACE, &content);

        let output = self.dest_dir.join("rsbinder_generated_aidl.rs");
        // println!("==== {output:?} ====");
        fs::write(output, content)?;

        Ok(())
    }
}
