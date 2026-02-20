// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// rsbinder-aidl is a build-time AIDL compiler; large error types on the stack
// have no meaningful performance impact, so suppress this clippy lint crate-wide.
#![allow(clippy::result_large_err)]

// #[macro_use]
// extern crate lazy_static;

use miette::{NamedSource, SourceSpan};
use std::collections::HashSet;
use std::fs;
use std::mem::take;
use std::path::{Path, PathBuf};

mod const_expr;
pub mod error;
mod generator;
mod parser;
mod type_generator;
pub use error::AidlError;
pub use generator::Generator;
pub use parser::parse_document;
pub use parser::SourceContext;

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
    includes: Vec<PathBuf>,
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
            includes: Vec::new(),
            dest_dir: PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or("aidl_gen".into())),
            output: "rsbinder_generated_aidl.rs".into(),
            enabled_async: cfg!(feature = "async"),
            is_crate: false,
        }
    }

    pub fn source(mut self, source: impl AsRef<Path>) -> Self {
        self.sources.push(source.as_ref().into());
        self
    }

    pub fn include_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.includes.push(dir.as_ref().into());
        self
    }

    pub fn output(mut self, output: impl AsRef<Path>) -> Self {
        let mut output = output.as_ref().to_owned();

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

    fn parse_file(filename: &Path) -> Result<(String, parser::Document, parser::SourceContext), AidlError> {
        println!("Parsing: {filename:?}");
        let source = fs::read_to_string(filename)?;
        let name = filename
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("invalid filename: {filename:?}"),
                )
            })?
            .to_string();
        let ctx = parser::SourceContext::new(filename.to_string_lossy().as_ref(), source);
        let document = parser::parse_document(&ctx)?;
        Ok((name, document, ctx))
    }

    fn generate_all(
        &self,
        mut package_list: Vec<(String, String, String)>,
    ) -> Result<String, AidlError> {
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
                    content += &format!("pub mod {mod} {{\n");
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

    fn parse_sources(&mut self) -> Result<Vec<(String, parser::Document, parser::SourceContext)>, AidlError> {
        let mut sources = take(&mut self.sources);
        let mut seen = HashSet::new();
        let mut includes = take(&mut self.includes).into_iter().collect::<HashSet<_>>();
        let mut document_list = Vec::new();
        let mut errors = Vec::new();

        fn strip_package(path: &Path, package: &str) -> Option<PathBuf> {
            let mut components = path.components();
            for package in package.split('.').rev() {
                if components.next_back()?.as_os_str().to_str()? != package {
                    return None;
                }
            }
            Some(components.collect())
        }

        while !sources.is_empty() {
            for path in take(&mut sources) {
                if seen.contains(&path) {
                    continue;
                }

                if path.is_file() {
                    match Self::parse_file(&path) {
                        Ok((name, doc, ctx)) => {
                            if let Some(dir) = doc
                                .package
                                .as_ref()
                                .and_then(|p| strip_package(path.parent()?, p))
                            {
                                includes.insert(dir);
                            }

                            for import in doc.imports.values() {
                                let rel_path =
                                    PathBuf::from(import.replace('.', "/")).with_extension("aidl");
                                let mut found = false;
                                for include_dir in &includes {
                                    let path = include_dir.join(&rel_path);
                                    if path.exists() {
                                        sources.push(path);
                                        found = true;
                                        break;
                                    }
                                }

                                if !found {
                                    // The exact byte offset of an import statement is not preserved in the AST,
                                    // so search the source text for the import string to approximate the span.
                                    let source_text =
                                        fs::read_to_string(&path).unwrap_or_default();
                                    let import_offset = source_text.find(import).unwrap_or(0);
                                    let import_len =
                                        if import_offset > 0 { import.len() } else { 0 };
                                    errors.push(AidlError::from(
                                        error::ResolutionError::ImportNotFound {
                                            import: import.clone(),
                                            src: NamedSource::new(
                                                path.to_string_lossy().as_ref(),
                                                source_text,
                                            ),
                                            span: SourceSpan::new(
                                                import_offset.into(),
                                                import_len,
                                            ),
                                        },
                                    ));
                                }
                            }

                            document_list.push((name, doc, ctx));
                        }
                        Err(e) => {
                            errors.push(e);
                        }
                    }
                } else {
                    let entries = fs::read_dir(&path).map_err(|err| {
                        std::io::Error::new(
                            err.kind(),
                            format!("parse_sources: fs::read_dir({path:?}) failed: {err}"),
                        )
                    })?;

                    for entry in entries {
                        let path = entry.unwrap().path();
                        if path.is_dir()
                            || (path.is_file() && path.extension().unwrap_or_default() == "aidl")
                        {
                            sources.push(path);
                        }
                    }
                };

                seen.insert(path);
            }
        }

        // If there are parse errors, report them immediately without semantic analysis (prevents cascading errors)
        if let Some(err) = AidlError::collect(errors) {
            return Err(err);
        }

        Ok(document_list)
    }

    pub fn generate(mut self) -> Result<(), AidlError> {
        let documents = self.parse_sources()?;

        // 1st pass: pre-register all enum symbols across all documents
        // so that parcelable default values can resolve enum references
        // regardless of file processing order.
        for document in &documents {
            generator::Generator::pre_register_enums(&document.1);
        }

        // 2nd pass: generate code, collecting errors across files
        let mut package_list = Vec::new();
        let mut errors = Vec::new();
        for document in &documents {
            println!("Generating: {}", document.0);
            // Re-establish the source context so that semantic errors generated
            // during code generation can reference the correct file name and source text.
            let _guard = parser::SourceGuard::new(&document.2.filename, &document.2.source);
            let gen = generator::Generator::new(self.enabled_async, self.is_crate);
            match gen.document(&document.1) {
                Ok(package) => {
                    package_list.push((package.0, package.1, document.0.clone()));
                }
                Err(e) => {
                    errors.push(e);
                }
            }
        }

        if let Some(err) = AidlError::collect(errors) {
            return Err(err);
        }

        let content = self.generate_all(package_list)?;

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
