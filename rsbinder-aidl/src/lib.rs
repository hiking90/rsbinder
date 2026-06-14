// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
// #[macro_use]
// extern crate lazy_static;

use miette::{NamedSource, SourceSpan};
use std::collections::{HashMap, HashSet};
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

        // Escape any path segment that is a Rust keyword (an AIDL type used as
        // a module here, e.g. a parcelable named `match`) so the reference
        // compiles; non-keyword segments are unchanged.
        let target_path = target_ns
            .iter()
            .map(|seg| escape_rust_keyword(seg))
            .collect::<Vec<_>>()
            .join(Self::RUST);
        "super::".repeat(curr_ns.len()) + &target_path
    }
}

/// Fully-qualified AIDL type names that the AOSP toolchain treats as
/// **framework-builtin primitives** rather than user-defined parcelables —
/// they are backed by the rsbinder runtime (`type_generator` maps them
/// to native Rust types), so `import` statements for them do not need a
/// resolvable `.aidl` source file alongside the vendored AOSP `.aidl`s.
///
/// Only fully-qualified names listed here are exempted; any unknown
/// import still surfaces as `ResolutionError::ImportNotFound`.
pub(crate) fn is_builtin_aidl_type(fqcn: &str) -> bool {
    matches!(fqcn, "android.os.ParcelFileDescriptor")
}

/// Wrap `ident` as a Rust raw identifier (`r#ident`) iff it is a Rust keyword
/// that would otherwise fail to compile as a plain identifier.
///
/// AIDL's identifier grammar permits type and member names that are Rust
/// keywords (`type`, `loop`, `match`, `move`, `impl`, …); the generated
/// `mod` / `struct` / `trait` declarations and the `::`-joined reference paths
/// that name them must escape those, exactly as AOSP's Rust backend does
/// (`pub struct r#<Name>`, `::r#<segment>`). `crate`, `self`, `Self` and
/// `super` cannot be raw identifiers — and are meaningful path keywords — so
/// they are returned unchanged. Non-keyword identifiers are returned unchanged,
/// so generated output for ordinary names is byte-for-byte identical.
pub(crate) fn escape_rust_keyword(ident: &str) -> std::borrow::Cow<'_, str> {
    // Strict + reserved Rust 2021 keywords, minus `crate`/`self`/`Self`/`super`
    // (invalid as raw identifiers; never need escaping in our output).
    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern",
        "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "pub", "ref", "return", "static", "struct", "trait", "true", "type", "unsafe", "use",
        "where", "while", "abstract", "become", "box", "do", "final", "macro", "override", "priv",
        "typeof", "unsized", "virtual", "yield", "try",
    ];
    if KEYWORDS.contains(&ident) {
        std::borrow::Cow::Owned(format!("r#{ident}"))
    } else {
        std::borrow::Cow::Borrowed(ident)
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

/// Per-source frozen-API metadata, populated by [`Builder::version`] /
/// [`Builder::hash`]. Mirrors AOSP `aidl --version N --hash <s>` semantics:
/// the version int and hash string are echoed verbatim through the
/// generated `getInterfaceVersion()` / `getInterfaceHash()` meta methods —
/// the generator does not compute or validate either value (AIDL API
/// snapshot freeze is a separate workflow).
#[derive(Default, Clone, Debug)]
struct VersionMeta {
    version: Option<i32>,
    hash: Option<String>,
}

pub struct Builder {
    sources: Vec<PathBuf>,
    includes: Vec<PathBuf>,
    dest_dir: PathBuf,
    output: PathBuf,
    enabled_async: bool,
    is_crate: bool,
    /// Per-source version/hash overrides. Keyed by the source path passed
    /// to [`Builder::source`]. [`Builder::version`] and [`Builder::hash`]
    /// apply to the most recently added source.
    version_meta: HashMap<PathBuf, VersionMeta>,
    /// Paths recorded during the parse phase for
    /// `cargo:rerun-if-changed=` emission in [`Builder::generate`].
    /// Captures every `.aidl` file that contributed to the generated
    /// output (initial sources + transitively resolved imports) plus
    /// every directory walked during resolution (user-supplied
    /// `include_dir`s, source paths that resolve to a directory, and
    /// package-derived include paths). cargo scans directories
    /// recursively, so the file-level + dir-level entries together
    /// trigger reruns on both modifications and additions/removals.
    dependencies: Vec<PathBuf>,
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
            version_meta: HashMap::new(),
            dependencies: Vec::new(),
        }
    }

    pub fn source(mut self, source: impl AsRef<Path>) -> Self {
        self.sources.push(source.as_ref().into());
        self
    }

    /// Stamp the **most recently added** source with an interface version,
    /// equivalent to AOSP `aidl --version N`. Causes the generator to emit
    /// `pub const VERSION: i32 = N;` plus a synthetic `getInterfaceVersion()`
    /// meta method (transaction code `FIRST_CALL_TRANSACTION + 0xFFFFFE`)
    /// for every interface declared in that source. Pair with
    /// [`Builder::hash`] to also emit `getInterfaceHash()`.
    ///
    /// Panics if no source has been added yet or if `v <= 0` (matches
    /// AOSP `aidl.cpp:615` which silently ignores non-positive versions —
    /// surfacing it here as an explicit failure avoids the silent-no-op
    /// trap that Tera's falsy-`0` semantics would otherwise create).
    pub fn version(mut self, v: i32) -> Self {
        assert!(
            v > 0,
            "Builder::version: N must be > 0; omit the call for unversioned interfaces"
        );
        let last = self
            .sources
            .last()
            .cloned()
            .expect("Builder::version() called before any source()");
        self.version_meta.entry(last).or_default().version = Some(v);
        self
    }

    /// Stamp the **most recently added** source with an interface hash,
    /// equivalent to AOSP `aidl --hash <s>`. The string is echoed verbatim
    /// through the generated `getInterfaceHash()` meta method — generator
    /// does not validate it against the AIDL contents (AIDL API snapshot
    /// freeze is a separate workflow).
    ///
    /// Panics if no source has been added yet.
    pub fn hash(mut self, h: impl Into<String>) -> Self {
        let last = self
            .sources
            .last()
            .cloned()
            .expect("Builder::hash() called before any source()");
        self.version_meta.entry(last).or_default().hash = Some(h.into());
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

    fn parse_file(
        filename: &Path,
    ) -> Result<(String, parser::Document, parser::SourceContext), AidlError> {
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

    fn parse_sources(
        &mut self,
    ) -> Result<Vec<(String, parser::Document, parser::SourceContext)>, AidlError> {
        let mut sources = take(&mut self.sources);
        let mut seen = HashSet::new();
        let initial_includes = take(&mut self.includes);
        for dir in &initial_includes {
            self.dependencies.push(dir.clone());
        }
        let mut includes = initial_includes.into_iter().collect::<HashSet<_>>();
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
                            self.dependencies.push(path.clone());
                            if let Some(dir) = doc
                                .package
                                .as_ref()
                                .and_then(|p| strip_package(path.parent()?, p))
                            {
                                if includes.insert(dir.clone()) {
                                    self.dependencies.push(dir);
                                }
                            }

                            for import in doc.imports.values() {
                                // Framework-builtin types have no standalone
                                // `.aidl` source in AOSP (the AIDL toolchain
                                // resolves them as primitives backed by the
                                // runtime crate). Skip resolution so e.g.
                                // `android/os/IAccessor.aidl`'s import of
                                // `android.os.ParcelFileDescriptor` does not
                                // demand a stub file alongside the vendored
                                // AOSP sources.
                                if is_builtin_aidl_type(import) {
                                    continue;
                                }
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
                                    let source_text = fs::read_to_string(&path).unwrap_or_default();
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
                                            span: SourceSpan::new(import_offset.into(), import_len),
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
                    self.dependencies.push(path.clone());
                    let entries = fs::read_dir(&path).map_err(|err| {
                        std::io::Error::new(
                            err.kind(),
                            format!("parse_sources: fs::read_dir({path:?}) failed: {err}"),
                        )
                    })?;

                    for entry in entries {
                        let path = entry
                            .map_err(|err| {
                                std::io::Error::new(
                                    err.kind(),
                                    format!("parse_sources: dir entry in {path:?} failed: {err}"),
                                )
                            })?
                            .path();
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
        self.emit_rerun_if_changed();
        Self::emit_warnings(&documents);

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
            // Look up per-source version/hash. Document filename matches
            // the path passed to `.source()` (see `parse_file`), so a
            // PathBuf key roundtrips. Imported sources picked up during
            // resolution have no entry and fall through to `None`.
            let meta = self
                .version_meta
                .get(&PathBuf::from(&document.2.filename))
                .cloned()
                .unwrap_or_default();
            let gen = generator::Generator::new(self.enabled_async, self.is_crate)
                .with_version_meta(meta.version, meta.hash);
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

    /// Return the sorted, deduplicated paths recorded as build-script
    /// dependencies during the parse phase. Each entry is either a
    /// `.aidl` file that contributed to the generated output (initial
    /// sources + transitively resolved imports) or a directory that
    /// was walked during resolution (user-supplied `include_dir`s,
    /// [`Builder::source`] paths that resolved to a directory, and
    /// the `<include>/<package-path>` directory inferred from each
    /// parsed source's package declaration).
    ///
    /// This is the same set [`Builder::generate`] emits as
    /// `cargo:rerun-if-changed=` lines, exposed as a non-stdout API so
    /// tests and non-cargo build integrations can inspect dependency
    /// tracking without capturing stdout.
    pub fn collect_aidl_dependencies(mut self) -> Result<Vec<PathBuf>, AidlError> {
        self.parse_sources()?;
        Ok(self.dedup_dependencies())
    }

    /// Emit `cargo:rerun-if-changed=<path>` for every recorded
    /// dependency. cargo disables its default "scan the build script
    /// crate root" policy as soon as any `rerun-if-changed` line is
    /// emitted, so this is the sole signal tying `.aidl` edits to
    /// build-script reruns — `build.rs` itself remains auto-tracked by
    /// cargo regardless.
    fn emit_rerun_if_changed(&mut self) {
        for path in self.dedup_dependencies() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    /// Forward parser-emitted [`AidlWarning`](crate::error::AidlWarning)
    /// diagnostics (e.g. unknown annotations) to cargo as
    /// `cargo:warning=<msg>` lines so they surface in the build output
    /// without aborting compilation.
    fn emit_warnings(documents: &[(String, parser::Document, parser::SourceContext)]) {
        for (_name, doc, _ctx) in documents {
            for w in &doc.warnings {
                println!("cargo:warning={}", w.message);
            }
        }
    }

    fn dedup_dependencies(&mut self) -> Vec<PathBuf> {
        let mut deps = take(&mut self.dependencies);
        deps.sort();
        deps.dedup();
        deps
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
