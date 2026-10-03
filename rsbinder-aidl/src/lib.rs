// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! AIDL compiler for [rsbinder](https://crates.io/crates/rsbinder).
//!
//! Translates Android AIDL (`.aidl`) files into Rust source implementing the
//! binder interfaces, parcelables, unions, and enums they declare. It is
//! intended to be driven from a `build.rs` via [`Builder`]:
//!
//! ```no_run
//! use std::path::PathBuf;
//!
//! rsbinder_aidl::Builder::new()
//!     .source(PathBuf::from("aidl/hello/IHello.aidl")) // a file or a directory
//!     .output(PathBuf::from("hello.rs"))               // under OUT_DIR
//!     .generate()
//!     .unwrap_or_else(|err| {
//!         // miette renders the file/line/snippet diagnostics.
//!         eprintln!("{:?}", miette::Report::new(err));
//!         std::process::exit(1);
//!     });
//! ```
//!
//! The consuming crate then includes the generated module with rsbinder's
//! `include_aidl!` macro:
//!
//! ```text
//! rsbinder::include_aidl!("hello", crate::hello::IHello::*);
//! ```
//!
//! # Builder options
//!
//! - [`Builder::source`] — add a `.aidl` file, or a directory scanned
//!   recursively for `*.aidl`. May be called multiple times.
//! - [`Builder::include_dir`] — add an import search directory (AOSP `-I`
//!   equivalent). All directories (user-supplied first, then package-derived
//!   ones inferred from parsed sources) are scanned deterministically; an
//!   import that resolves under more than one directory is rejected as
//!   ambiguous, matching AOSP.
//! - [`Builder::output`] — the generated file name, written under
//!   `OUT_DIR` (falls back to `aidl_gen/` outside cargo).
//! - [`Builder::version`] / [`Builder::hash`] — stamp the **most recently
//!   added file source** with stable-AIDL version metadata (AOSP
//!   `aidl --version N --hash <s>` equivalent), emitting
//!   `getInterfaceVersion()` / `getInterfaceHash()` meta methods.
//! - [`Builder::set_async_support`] — also emit `.await`-able async
//!   client/server traits (defaults to the crate's `async` feature).
//!
//! # Constants
//!
//! Constant names are emitted **verbatim** — `const int kFoo` becomes
//! `r#kFoo`, with no case-normalization — and constant expressions are
//! evaluated with AOSP-strict rules: integer overflow, lossy narrowing,
//! circular constant references, and invalid shift amounts are compile
//! errors rather than being silently wrapped or truncated.
//!
//! # Validation
//!
//! The generator enforces AOSP `aidl`'s type-placement rules, not just the
//! ones it needs to emit code: a `@FixedSize` type's fields must all be fixed
//! size, a `@VintfStability` type may only reference `@VintfStability` types,
//! `ParcelableHolder` is not an
//! array/`List`/`@nullable`/argument/return/union-member type, and `void` is a
//! bare return type only. The `@FixedSize` and `@VintfStability` checks are
//! contract-level — rsbinder would generate compiling code either way — so
//! that an `.aidl` authored here is one AOSP's compiler also accepts; so are
//! the argument, return-type and union-member forms of `ParcelableHolder`. The
//! `void` placements and the array/`List`/`@nullable` forms of
//! `ParcelableHolder` have no compiling Rust representation at all, so the
//! check reports them as an AIDL diagnostic instead of a rustc error in the
//! generated crate — as do a `union` with no fields, a duplicate argument
//! name, and a type argument on a type that takes none (`String<int>`).
//!
//! A `@VintfStability` interface also declares that stability to the runtime,
//! so the binder it publishes is accepted by a peer that requires VINTF.
//!
//! A `const` must have a primitive or `String` type, or an array of those.
//! AOSP's set is narrower still (`{String, byte, int, long, float, double}`);
//! `boolean`, `char` and constant arrays are deliberate rsbinder extensions,
//! as `List<int>` is.
//!
//! Not checked: a nested type named `Vec`, `Box`, `Option`, `String`,
//! `Default`, `std` or `rsbinder`. The generated code spells those names
//! without a path next to the nested type's module, so the name resolves to
//! the nested type and the generated file fails with a rustc error instead of
//! an AIDL diagnostic; give the nested type another name. A generic
//! parcelable's type parameter with such a name is refused.
//!
//! # Deprecation
//!
//! A `/** @deprecated note */` javadoc block above a declaration, method,
//! field, constant, or enumerator becomes `#[deprecated = "note"]` on the
//! generated item, as in AOSP's Rust backend.
//!
//! Compatibility notes, supported AIDL constructs, and diagnostics examples
//! live in the repository README and <https://hiking90.github.io/rsbinder/>.

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

/// The code-render layer: the data the templates consume, plus the functions
/// that run them.
///
/// `rsbinder-aidl` fills these structs from parsed AIDL. They are public so a
/// second front-end can fill them from something else and get **identical**
/// generated code — that is how `#[rsbinder::interface]` (crate
/// `rsbinder-macros`, plan 2-19) turns a Rust trait into the same
/// `Bn*`/`Bp*`/trait set an `.aidl` would produce, without a second copy of
/// the templates.
///
/// The fragments in [`FnMembers`](render::FnMembers) are pre-rendered Rust
/// source, not AIDL types: serialization in the templates is trait-generic, so
/// nothing here needs to know what a value *is*, only how to write it.
pub mod render {
    pub use crate::generator::{
        deprecated_attr, function_names, interface_stem, render_enum, render_interface,
        render_parcelable, ConstMember, EnumMember, EnumRender, FnMembers, InterfaceRender,
        ParcelableMember, ParcelableRender, TransactionWrite, RESERVED_NAME_PREFIX,
    };
}
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

        // A segment may be a Rust keyword (e.g. a parcelable `match` used as a module): `r#` it.
        let target_path = target_ns
            .iter()
            .map(|seg| escape_rust_keyword(seg))
            .collect::<Vec<_>>()
            .join(Self::RUST);
        "super::".repeat(curr_ns.len()) + &target_path
    }
}

// Framework builtins backed by the rsbinder runtime: importing one needs no resolvable `.aidl`.
pub(crate) fn is_builtin_aidl_type(fqcn: &str) -> bool {
    matches!(fqcn, "android.os.ParcelFileDescriptor")
}

/// A declaration the runtime crate ships compiled: parsed, never generated (plans/12-fmq.md §10.1).
pub(crate) struct BuiltinDecl {
    /// Fully-qualified AIDL name.
    pub fqcn: &'static str,
    /// Where the runtime crate exposes the type, relative to its root.
    pub rust_path: &'static str,
    /// Path of the vendored source, for diagnostics.
    pub filename: &'static str,
    /// The vendored AOSP source text.
    pub source: &'static str,
}

/// AOSP FMQ types vendored from `android17-release` (frozen V2, `common/fmq/aidl`), plus rsbinder's.
pub(crate) const BUILTIN_DECLS: &[BuiltinDecl] = &[
    BuiltinDecl {
        fqcn: "android.hardware.common.NativeHandle",
        rust_path: "NativeHandle",
        filename: "<rsbinder-aidl>/android/hardware/common/NativeHandle.aidl",
        source: include_str!("../aidl/android/hardware/common/NativeHandle.aidl"),
    },
    BuiltinDecl {
        fqcn: "android.hardware.common.fmq.GrantorDescriptor",
        rust_path: "fmq::GrantorDescriptor",
        filename: "<rsbinder-aidl>/android/hardware/common/fmq/GrantorDescriptor.aidl",
        source: include_str!("../aidl/android/hardware/common/fmq/GrantorDescriptor.aidl"),
    },
    BuiltinDecl {
        fqcn: "android.hardware.common.fmq.MQDescriptor",
        rust_path: "fmq::MQDescriptor",
        filename: "<rsbinder-aidl>/android/hardware/common/fmq/MQDescriptor.aidl",
        source: include_str!("../aidl/android/hardware/common/fmq/MQDescriptor.aidl"),
    },
    BuiltinDecl {
        fqcn: "android.hardware.common.fmq.SynchronizedReadWrite",
        rust_path: "fmq::SynchronizedReadWrite",
        filename: "<rsbinder-aidl>/android/hardware/common/fmq/SynchronizedReadWrite.aidl",
        source: include_str!("../aidl/android/hardware/common/fmq/SynchronizedReadWrite.aidl"),
    },
    BuiltinDecl {
        fqcn: "android.hardware.common.fmq.UnsynchronizedWrite",
        rust_path: "fmq::UnsynchronizedWrite",
        filename: "<rsbinder-aidl>/android/hardware/common/fmq/UnsynchronizedWrite.aidl",
        source: include_str!("../aidl/android/hardware/common/fmq/UnsynchronizedWrite.aidl"),
    },
    // rsbinder's own: a stream's consumer end (`rsbinder::stream`), passed in the opening call.
    BuiltinDecl {
        fqcn: "rsbinder.stream.StreamEndpoint",
        rust_path: "stream::StreamEndpoint",
        filename: "<rsbinder-aidl>/rsbinder/stream/StreamEndpoint.aidl",
        source: include_str!("../aidl/rsbinder/stream/StreamEndpoint.aidl"),
    },
];

pub(crate) fn builtin_decl(fqcn: &str) -> Option<&'static BuiltinDecl> {
    BUILTIN_DECLS.iter().find(|decl| decl.fqcn == fqcn)
}

/// Refuses a builtin vendored under two include dirs, as AOSP does ("Duplicate files found").
fn ambiguous_builtin_copy(fqcn: &str, candidates: &[PathBuf]) -> AidlError {
    AidlError::Config {
        message: format!(
            "'{fqcn}' found under more than one include directory: {}",
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn import_candidates(includes: &[PathBuf], import: &str) -> Vec<PathBuf> {
    let rel_path = PathBuf::from(import.replace('.', "/")).with_extension("aidl");
    includes
        .iter()
        .map(|dir| dir.join(&rel_path))
        .filter(|p| p.exists())
        .collect()
}

/// AOSP `ImportResolver::FindImportFile`: `p.IOuter.Inner` may be declared in `p/IOuter.aidl`.
fn enclosing_import_candidates(includes: &[PathBuf], import: &str) -> Vec<PathBuf> {
    let mut parts: Vec<&str> = import.split('.').collect();
    while parts.len() > 1 {
        parts.pop();
        let found = import_candidates(includes, &parts.join("."));
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

// A builtin's fqcn is never shortened: `p.IOuter` must not stand in for builtin `p.IOuter.T`.
fn resolve_import(includes: &[PathBuf], import: &str) -> Vec<PathBuf> {
    let candidates = import_candidates(includes, import);
    if candidates.is_empty() && builtin_decl(import).is_none() {
        return enclosing_import_candidates(includes, import);
    }
    candidates
}

// The AST drops import offsets; approximate by text search.
fn import_span(path: &Path, import: &str) -> (NamedSource<String>, SourceSpan) {
    let source_text = fs::read_to_string(path).unwrap_or_default();
    let offset = source_text.find(import).unwrap_or(0);
    let len = if offset > 0 { import.len() } else { 0 };
    (
        NamedSource::new(path.to_string_lossy().as_ref(), source_text),
        SourceSpan::new(offset.into(), len),
    )
}

fn ambiguous_import(path: &Path, import: &str, candidates: &[PathBuf]) -> AidlError {
    let (src, span) = import_span(path, import);
    AidlError::from(error::ResolutionError::AmbiguousImport {
        import: import.to_owned(),
        candidates: candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        src,
        span,
    })
}

// `r#`-escapes as AOSP does; `crate`/`self`/`Self`/`super` are rejected in the parser.
pub(crate) fn escape_rust_keyword(ident: &str) -> std::borrow::Cow<'_, str> {
    // Strict + reserved keywords through Rust 2024: output compiles in the consumer's edition.
    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern",
        "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "pub", "ref", "return", "static", "struct", "trait", "true", "type", "unsafe", "use",
        "where", "while", "abstract", "become", "box", "do", "final", "macro", "override", "priv",
        "typeof", "unsized", "virtual", "yield", "try", "gen",
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

/// Per-source AOSP `aidl --version N --hash <s>`: echoed verbatim, never computed or validated.
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
    trace: bool,
    /// Per-source version/hash, keyed by the path passed to [`Builder::source`].
    version_meta: HashMap<PathBuf, VersionMeta>,
    // Contributing `.aidl` files plus walked dirs: cargo rescans dirs, so additions rerun too.
    dependencies: Vec<PathBuf>,
    // Builtin declarations an import pulled in: parsed so references resolve, never generated.
    builtin_documents: Vec<parser::Document>,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
            includes: Vec::new(),
            dest_dir: PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or("aidl_gen".into())),
            output: "rsbinder_generated_aidl.rs".into(),
            enabled_async: cfg!(feature = "async"),
            is_crate: false,
            trace: false,
            version_meta: HashMap::new(),
            dependencies: Vec::new(),
            builtin_documents: Vec::new(),
        }
    }

    pub fn source(mut self, source: impl AsRef<Path>) -> Self {
        self.sources.push(source.as_ref().into());
        self
    }

    /// Directory [`Builder::output`] is resolved against, overriding the
    /// `OUT_DIR` environment variable this builder otherwise reads.
    ///
    /// `OUT_DIR` is process-wide, so a caller outside a `build.rs` — a test
    /// generating into a temporary directory, a tool driving several builders
    /// — would have to mutate the environment to steer the output, which is
    /// not thread-safe. Set the directory here instead.
    pub fn dest_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.dest_dir = dir.as_ref().into();
        self
    }

    /// Stamp the **most recently added** source with an interface version,
    /// equivalent to AOSP `aidl --version N`. Causes the generator to emit
    /// `pub const VERSION: i32 = N;` plus a synthetic `getInterfaceVersion()`
    /// meta method (transaction code `FIRST_CALL_TRANSACTION + 0xFFFFFE`)
    /// for every interface declared in that source. Pair with
    /// [`Builder::hash`] to also emit `getInterfaceHash()`.
    ///
    /// An interface annotated `@VersionSupport(version = N)` is versioned
    /// without this call; with it, the two versions must match (AOSP
    /// `AidlInterface::Version` / `VersionSpecificCheckValid`).
    ///
    /// Panics if no source has been added yet, if the preceding source is a
    /// directory, or if `v <= 0` (AOSP `options.cpp` refuses `--version` ≤ 0
    /// the same way).
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
        // Keyed by exact file path: a directory's files would never match, dropping the version.
        assert!(
            !last.is_dir(),
            "Builder::version() applies to a single .aidl file source, but the preceding \
             source() is a directory: {last:?}"
        );
        self.version_meta.entry(last).or_default().version = Some(v);
        self
    }

    /// Stamp the **most recently added** source with an interface hash,
    /// equivalent to AOSP `aidl --hash <s>`. The string is echoed verbatim
    /// through the generated `getInterfaceHash()` meta method — generator
    /// does not validate it against the AIDL contents (AIDL API snapshot
    /// freeze is a separate workflow).
    ///
    /// Panics if no source has been added yet, if the preceding source is a
    /// directory, or if `h` is empty (an empty hash is falsy to Tera and would
    /// silently emit no `getInterfaceHash()`).
    pub fn hash(mut self, h: impl Into<String>) -> Self {
        let last = self
            .sources
            .last()
            .cloned()
            .expect("Builder::hash() called before any source()");
        // See `version()`: directory sources never match the per-file key.
        assert!(
            !last.is_dir(),
            "Builder::hash() applies to a single .aidl file source, but the preceding \
             source() is a directory: {last:?}"
        );
        let h = h.into();
        assert!(
            !h.is_empty(),
            "Builder::hash: the hash must be non-empty; omit the call for unhashed interfaces"
        );
        self.version_meta.entry(last).or_default().hash = Some(h);
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
        self
    }

    /// Emit a method-name table for every interface, equivalent to AOSP
    /// `aidl --trace`. The generated service then answers
    /// `rsbinder::Remotable::transaction_name(code)` with the AIDL method
    /// name, which rsbinder uses to name transactions in traces and
    /// transaction observers, and generated proxies open the client span
    /// around each call. Without it only the server span exists, named by
    /// code, and no client span is opened (AOSP opens the client section
    /// regardless of `--trace`).
    /// Applies to all sources. Off by default, as in AOSP: the table adds one
    /// string per method to the binary. The wire format is unaffected.
    pub fn trace(mut self, enable: bool) -> Self {
        self.trace = enable;
        self
    }

    fn parse_file(
        filename: &Path,
    ) -> Result<(String, parser::Document, parser::SourceContext), AidlError> {
        println!("Parsing: {filename:?}");
        let source = fs::read_to_string(filename).map_err(|err| {
            std::io::Error::new(err.kind(), format!("cannot read {filename:?}: {err}"))
        })?;
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
                    // Outer attribute: lints on the package module itself (`module_inception`).
                    if mod_count == 0 {
                        content += "#[allow(clippy::all, reason = \"rsbinder-aidl generated code\")]\n#[allow(unused_imports, reason = \"rsbinder-aidl generated code\")]\n";
                    }
                    content += &indent_space(mod_count);
                    content += &format!("pub mod {} {{\n", escape_rust_keyword(r#mod));
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

    /// Parse a builtin's vendored source and register its runtime path (plans/12-fmq.md §10.1).
    fn add_builtin(
        &mut self,
        builtin: &'static BuiltinDecl,
        includes: &[PathBuf],
        sources: &mut Vec<PathBuf>,
        pending: &mut Vec<&'static BuiltinDecl>,
    ) -> Result<(), AidlError> {
        let ns = Namespace::new(builtin.fqcn, Namespace::AIDL);
        if parser::builtin_rust_path(&ns).is_some() || parser::is_declared(&ns) {
            return Ok(());
        }
        parser::register_builtin_path(&ns, builtin.rust_path);
        let ctx = parser::SourceContext::new(builtin.filename, builtin.source);
        let doc = parser::parse_document(&ctx)?;
        for import in doc.imports.values() {
            if is_builtin_aidl_type(import) {
                continue;
            }
            match <[PathBuf; 1]>::try_from(import_candidates(includes, import)) {
                Ok([only]) => sources.push(only),
                Err(none) if none.is_empty() => {
                    let dependency = builtin_decl(import).ok_or_else(|| AidlError::Config {
                        message: format!(
                            "builtin '{}' imports '{import}', which is not a builtin",
                            builtin.fqcn
                        ),
                    })?;
                    pending.push(dependency);
                }
                Err(candidates) => return Err(ambiguous_builtin_copy(import, &candidates)),
            }
        }
        self.builtin_documents.push(doc);
        Ok(())
    }

    fn parse_sources(
        &mut self,
    ) -> Result<Vec<(String, parser::Document, parser::SourceContext)>, AidlError> {
        // Reset here, not in `new()`: two builders built before either generates share the table.
        parser::reset();
        self.builtin_documents.clear();
        let mut sources = take(&mut self.sources);
        let mut seen = HashSet::new();
        // User dirs, then package-derived; an import in two is ambiguous (AOSP import_resolver).
        let mut includes: Vec<PathBuf> = Vec::new();
        // Canonical key: `./aidl` == `aidl`; `""` (source right under its package path) is cwd.
        fn name_the_cwd(dir: PathBuf) -> PathBuf {
            if dir.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                dir
            }
        }
        let include_key = |dir: &Path| fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let mut include_seen: HashSet<PathBuf> = HashSet::new();
        for dir in take(&mut self.includes) {
            let dir = name_the_cwd(dir);
            if include_seen.insert(include_key(&dir)) {
                self.dependencies.push(dir.clone());
                includes.push(dir);
            }
        }
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

        // Builtins wait for every source: an include dir may vendor a name a builtin imports.
        let mut pending_builtins: Vec<&'static BuiltinDecl> = Vec::new();
        // (importing file, import, chosen file); rechecked once every include dir is known.
        let mut resolved: Vec<(PathBuf, String, PathBuf)> = Vec::new();
        // (importing file, import) not yet resolved; a later source may add its include dir.
        let mut unresolved: Vec<(PathBuf, String)> = Vec::new();
        while !sources.is_empty() || !pending_builtins.is_empty() {
            for path in take(&mut sources) {
                // Canonicalise: a symlink cycle (`aidl/loop -> .`) yields endless new paths.
                let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                if !seen.insert(key) {
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
                                let dir = name_the_cwd(dir);
                                if include_seen.insert(include_key(&dir)) {
                                    includes.push(dir.clone());
                                    self.dependencies.push(dir);
                                }
                            }

                            for import in doc.imports.values() {
                                // AOSP has no `.aidl` for these (e.g. IAccessor's PFD).
                                if is_builtin_aidl_type(import) {
                                    continue;
                                }
                                unresolved.push((path.clone(), import.clone()));
                            }

                            document_list.push((name, doc, ctx));
                        }
                        Err(e) => {
                            errors.push(e);
                        }
                    }
                } else if !path.is_dir() {
                    // A mistyped `source()` path: report it with the other errors, not as read_dir.
                    errors.push(AidlError::from(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("source {path:?} does not exist"),
                    )));
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
            }
            if !sources.is_empty() {
                continue;
            }
            // Resolve once the queued sources are parsed: AOSP's exact-file rank ignores order.
            unresolved.retain(|(path, import)| {
                match <[PathBuf; 1]>::try_from(resolve_import(&includes, import)) {
                    Ok([chosen]) => {
                        sources.push(chosen.clone());
                        resolved.push((path.clone(), import.clone(), chosen));
                    }
                    Err(none) if none.is_empty() => match builtin_decl(import) {
                        Some(builtin) => pending_builtins.push(builtin),
                        None => return true,
                    },
                    Err(candidates) => errors.push(ambiguous_import(path, import, &candidates)),
                }
                false
            });
            if !sources.is_empty() {
                continue;
            }
            if let Some(builtin) = pending_builtins.pop() {
                // Look again: every source since this import was met added its package dir.
                match <[PathBuf; 1]>::try_from(import_candidates(&includes, builtin.fqcn)) {
                    Ok([only]) => {
                        sources.push(only);
                        continue;
                    }
                    Err(none) if none.is_empty() => {}
                    Err(candidates) => {
                        errors.push(ambiguous_builtin_copy(builtin.fqcn, &candidates));
                        continue;
                    }
                }
                // Collected, not returned: the sources' diagnostics above share the report.
                if let Err(e) =
                    self.add_builtin(builtin, &includes, &mut sources, &mut pending_builtins)
                {
                    errors.push(e);
                }
            }
        }

        for (path, import) in &unresolved {
            let (src, span) = import_span(path, import);
            errors.push(AidlError::from(error::ResolutionError::ImportNotFound {
                import: import.clone(),
                src,
                span,
            }));
        }

        // A dir added after an import resolved can make it ambiguous, in any `source()` order.
        for (path, import, chosen) in &resolved {
            let mut candidates = resolve_import(&includes, import);
            if candidates.len() == 1 && candidates[0] != *chosen {
                // A later dir's exact file outranks the enclosing file already compiled.
                candidates.insert(0, chosen.clone());
            }
            if candidates.len() > 1 {
                errors.push(ambiguous_import(path, import, &candidates));
            }
        }

        // A copy seen after its builtin was registered is refused (plans/12-fmq.md §10.3).
        for builtin in BUILTIN_DECLS {
            let ns = Namespace::new(builtin.fqcn, Namespace::AIDL);
            if parser::builtin_rust_path(&ns).is_none() {
                continue;
            }
            let candidates = import_candidates(&includes, builtin.fqcn);
            if !candidates.is_empty() {
                errors.push(AidlError::Config {
                    message: format!(
                        "'{}' was resolved to the runtime crate's type before its source {} \
                         became visible; add Builder::include_dir() for that directory so \
                         the copy is compiled instead",
                        builtin.fqcn,
                        candidates
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
        }

        // Parse errors stop here: semantic analysis on them would only cascade.
        if let Some(err) = AidlError::collect(errors) {
            return Err(err);
        }

        Ok(document_list)
    }

    pub fn generate(mut self) -> Result<(), AidlError> {
        let documents = self.parse_sources()?;
        // An empty output would defer this build-script typo to an include_aidl! import error.
        if documents.is_empty() {
            return Err(AidlError::Config {
                message: "no .aidl sources found: add Builder::source(<file-or-dir>) entries \
                          (directories are scanned recursively for *.aidl)"
                    .into(),
            });
        }
        // A meta key matching no parsed file would silently emit an unversioned interface.
        for meta_path in self.version_meta.keys() {
            if !documents
                .iter()
                .any(|doc| Path::new(&doc.2.filename) == meta_path)
            {
                return Err(AidlError::Config {
                    message: format!(
                        "version()/hash() was applied to source {meta_path:?}, which is not \
                         among the parsed .aidl files"
                    ),
                });
            }
        }
        // AOSP `AidlTypenames` "redefinition": one qualified name declared in two files.
        let mut defined: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
        for document in &documents {
            let package = document.1.package.as_deref().unwrap_or_default();
            for decl in &document.1.decls {
                let qualified = if package.is_empty() {
                    decl.name().to_string()
                } else {
                    format!("{package}.{}", decl.name())
                };
                // Within one file `Generator::document` reports it, with a span.
                let Some(earlier) = defined.insert(qualified.clone(), &document.2.filename) else {
                    continue;
                };
                if earlier != document.2.filename {
                    return Err(AidlError::Config {
                        message: format!(
                            "type '{qualified}' is defined in both {earlier} and {}",
                            document.2.filename
                        ),
                    });
                }
            }
        }
        self.emit_rerun_if_changed();
        Self::emit_warnings(&documents);

        // 1st pass: enums first, so defaults resolve enum references in any file order.
        for document in &self.builtin_documents {
            generator::Generator::pre_register_enums(document);
        }
        for document in &documents {
            generator::Generator::pre_register_enums(&document.1);
        }

        // 2nd pass: generate code, collecting errors across files
        let mut package_list = Vec::new();
        let mut errors = Vec::new();
        for document in &documents {
            println!("Generating: {}", document.0);
            // Semantic errors raised during generation need this file's name and text.
            let _guard = parser::SourceGuard::new(&document.2.filename, &document.2.source);
            // Filename == the `.source()` path (see `parse_file`); imported sources get `None`.
            let meta = self
                .version_meta
                .get(&PathBuf::from(&document.2.filename))
                .cloned()
                .unwrap_or_default();
            let gen = generator::Generator::new(self.enabled_async, self.is_crate)
                .with_version_meta(meta.version, meta.hash)
                .with_trace(self.trace);
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

        let out_path = self.dest_dir.join(&self.output);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                std::io::Error::new(
                    err.kind(),
                    format!("cannot create output directory {parent:?}: {err}"),
                )
            })?;
        }
        fs::write(&out_path, content).map_err(|err| {
            std::io::Error::new(
                err.kind(),
                format!("cannot write generated output {out_path:?}: {err}"),
            )
        })?;

        Ok(())
    }

    /// Return the sorted, deduplicated paths recorded as build-script
    /// dependencies during the parse phase. Each entry is either a
    /// `.aidl` file that contributed to the generated output (initial
    /// sources + transitively resolved imports) or a directory that
    /// was walked during resolution (user-supplied `include_dir`s,
    /// [`Builder::source`] paths that resolved to a directory, and
    /// the include root inferred from each parsed source's package
    /// declaration — its directory with the package segments stripped).
    ///
    /// This is the same set [`Builder::generate`] emits as
    /// `cargo:rerun-if-changed=` lines, exposed as a non-stdout API so
    /// tests and non-cargo build integrations can inspect dependency
    /// tracking without capturing stdout.
    pub fn collect_aidl_dependencies(mut self) -> Result<Vec<PathBuf>, AidlError> {
        self.parse_sources()?;
        Ok(self.dedup_dependencies())
    }

    /// Any `rerun-if-changed` disables cargo's default scan, so this is the only `.aidl` signal.
    fn emit_rerun_if_changed(&mut self) {
        for path in self.dedup_dependencies() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    /// Parser warnings become `cargo:warning=` lines: visible, non-fatal.
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
