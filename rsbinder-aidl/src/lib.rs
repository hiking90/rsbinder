// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

#[macro_use]
extern crate lazy_static;

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

mod const_expr;
mod generator;
mod parser;
mod type_generator;
pub use generator::Generator;
pub use parser::parse_document;

#[derive(Default, Hash, Eq, PartialEq, Debug, Clone)]
pub struct Namespace {
    ns: Vec<String>,
}

impl Namespace {
    pub const AIDL: &'static str = ".";
    pub const RUST: &'static str = "::";

    pub fn new(namespace: &str, style: &str) -> Self {
        Self {
            ns: namespace.split(style).map(|s| s.into()).collect(),
        }
    }

    pub fn push(&mut self, name: &str) {
        self.ns.push(name.into())
    }

    pub fn push_ns(&mut self, ns: &Namespace) {
        self.ns.extend_from_slice(&ns.ns);
    }

    pub fn pop(&mut self) -> Option<String> {
        self.ns.pop()
    }

    pub fn to_string(&self, style: &str) -> String {
        self.ns.join(style)
    }

    pub fn relative_mod(&self, target: &Namespace) -> String {
        let mut curr_ns = self.ns.clone();
        let mut target_ns = target.ns.clone();

        let mut index_to_remove = 0;

        for (item1, item2) in curr_ns.iter().zip(target_ns.iter()) {
            if item1 == item2 {
                index_to_remove += 1;
            } else {
                break;
            }
        }

        curr_ns.drain(0..index_to_remove);
        target_ns.drain(0..index_to_remove);

        "super::".repeat(curr_ns.len()) + &target_ns.join(Self::RUST)
    }
}

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
        if !line.is_empty() {
            content += &(indent_space(step) + line + "\n");
        } else {
            content += "\n";
        }
    }
    content
}

pub struct Builder {
    sources: Vec<PathBuf>,
    dest_dir: PathBuf,
    output: PathBuf,
    enabled_async: bool,
    is_crate: bool,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Self {
        parser::reset();
        Self {
            sources: Vec::new(),
            dest_dir: PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or("aidl_gen".into())),
            output: "rsbinder_generated_aidl.rs".into(),
            enabled_async: false,
            is_crate: false,
        }
    }

    pub fn source(mut self, source: PathBuf) -> Self {
        self.sources.push(source);
        self
    }

    pub fn output(mut self, mut output: PathBuf) -> Self {
        if output.extension().is_none() {
            output.set_extension("rs");
        }

        self.output = output;

        self
    }

    pub fn set_async_support(mut self, enable: bool) -> Self {
        self.enabled_async = enable;
        self
    }

    /// It must be used in rsbinder's build.rs.
    /// It generates the rust output file with crate::??? instead of rsbinder::???.
    pub fn set_crate_support(mut self, enable: bool) -> Self {
        self.is_crate = enable;
        type_generator::set_crate_support(enable);
        self
    }

    fn parse_file(filename: &Path) -> Result<(String, parser::Document), Box<dyn Error>> {
        println!("Parsing: {:?}", filename);
        let unparsed_file = fs::read_to_string(filename)?;
        let document = parser::parse_document(&unparsed_file)?;

        Ok((
            filename.file_stem().unwrap().to_str().unwrap().to_string(),
            document,
        ))
    }

    fn traverse_source(dir: &Path) -> Result<Vec<(String, parser::Document)>, Box<dyn Error>> {
        let entries = fs::read_dir(dir)
            .inspect_err(|_| {
                eprintln!("traverse_source: fs::read_dir({dir:?}) failed.");
            })
            .unwrap();
        let mut package_list = Vec::new();

        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                package_list.append(&mut Self::traverse_source(&path)?);
            }
            if path.is_file() && path.extension().unwrap_or_default() == "aidl" {
                let package = Self::parse_file(&path)?;
                package_list.push(package);
            }
        }

        Ok(package_list)
    }

    fn generate_all(
        &self,
        mut package_list: Vec<(String, String, String)>,
    ) -> Result<String, Box<dyn Error>> {
        let mut content = String::new();
        let mut namespace = String::new();
        let mut mod_count: usize = 0;

        content += "#[allow(clippy::all)]\n";
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
                let package = Self::parse_file(source)?;
                document_list.push(package);
            } else {
                document_list.append(&mut Self::traverse_source(source)?);
            };
        }

        let mut package_list = Vec::new();
        for document in document_list {
            println!("Generating: {}", document.0);
            let gen = generator::Generator::new(self.enabled_async, self.is_crate);
            let package = gen.document(&document.1)?;
            package_list.push((package.0, package.1, document.0));
        }

        let content = self.generate_all(package_list)?;
        // let content = add_namespace(DEFAULT_NAMESPACE, &content);

        fs::write(self.dest_dir.join(&self.output), content)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // use std::path::Path;
    // use std::fs;
    use super::*;

    #[test]
    fn test_relative_mod() {
        let target = Namespace::new("android.os.IServiceCallback", Namespace::AIDL);
        let curr = Namespace::new("android.os.IServiceManager", Namespace::AIDL);

        assert_eq!(curr.relative_mod(&target), "super::IServiceCallback");

        let target = Namespace::new("android.aidl.test.IServiceCallback", Namespace::AIDL);
        let curr = Namespace::new("android.os.IServiceManager", Namespace::AIDL);

        assert_eq!(
            curr.relative_mod(&target),
            "super::super::aidl::test::IServiceCallback"
        );
    }
}
