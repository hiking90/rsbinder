// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use crate::error::{pest_error_to_diagnostic, AidlError, ParseError};

use convert_case::{Case, Casing};

use pest::Parser;
#[derive(pest_derive::Parser)]
#[grammar = "aidl.pest"]
pub struct AIDLParser;

use crate::const_expr::{ConstExpr, ValueType};
use crate::type_generator;
use crate::Namespace;

thread_local! {
    static DECLARATION_MAP: RefCell<HashMap<Namespace, Declaration>> = RefCell::new(HashMap::new());
    static DECLARATION_DOCUMENT_MAP: RefCell<HashMap<Namespace, DocumentContext>> = RefCell::new(HashMap::new());
    static NAMESPACE_STACK: RefCell<Vec<Namespace>> = const { RefCell::new(Vec::new()) };
    static DOCUMENT: RefCell<Document> = RefCell::new(Document::new());

    // Universal Symbol Table - supports all types of named constants
    static SYMBOL_TABLE: RefCell<HashMap<String, ConstExpr>> = RefCell::new(HashMap::new());
    static ENUM_VALUE_CACHE: RefCell<HashMap<String, ConstExpr>> = RefCell::new(HashMap::new());
    static ENUM_RESOLUTION_STACK: RefCell<HashSet<String>> = RefCell::new(HashSet::new());

    // Filename and text of the source being parsed, for error diagnostics.
    static CURRENT_SOURCE_NAME: RefCell<String> = const { RefCell::new(String::new()) };
    static CURRENT_SOURCE_TEXT: RefCell<String> = const { RefCell::new(String::new()) };

    // Non-fatal diagnostics of the current `parse_document`, drained into `Document::warnings`.
    static CURRENT_WARNINGS: RefCell<Vec<crate::error::AidlWarning>> = const { RefCell::new(Vec::new()) };

    // Each `crate::BUILTIN_DECLS` entry: AIDL namespace -> Rust path relative to the runtime crate.
    static BUILTIN_RUST_PATHS: RefCell<HashMap<Namespace, String>> = RefCell::new(HashMap::new());
}

/// Record that `ns` is not generated but provided at the runtime crate's `rust_path`.
pub(crate) fn register_builtin_path(ns: &Namespace, rust_path: &str) {
    BUILTIN_RUST_PATHS.with(|map| {
        map.borrow_mut().insert(ns.clone(), rust_path.to_owned());
    });
}

/// The runtime-crate path of a builtin declaration; `None` when it is generated here.
pub(crate) fn builtin_rust_path(ns: &Namespace) -> Option<String> {
    BUILTIN_RUST_PATHS.with(|map| map.borrow().get(ns).cloned())
}

/// Whether a parsed document has declared `ns`.
pub(crate) fn is_declared(ns: &Namespace) -> bool {
    DECLARATION_MAP.with(|map| map.borrow().contains_key(ns))
}

/// AOSP `AllSchemas()` android-16 (`@JavaDefault`) ∪ android-17 (`@VersionSupport`); others warn.
const KNOWN_ANNOTATIONS: &[&str] = &[
    "@Backing",
    "@Descriptor",
    "@EnforcePermission",
    "@FixedSize",
    "@JavaDefault",
    "@JavaDelegator",
    "@JavaDerive",
    "@JavaOnlyImmutable",
    "@JavaOnlyStableParcelable",
    "@JavaPassthrough",
    "@JavaSuppressLint",
    "@NdkOnlyStableParcelable",
    "@PermissionManuallyEnforced",
    "@PropagateAllowBlocking",
    "@RequiresNoPermission",
    "@RustDerive",
    "@RustOnlyStableParcelable",
    "@SensitiveData",
    "@SuppressWarnings",
    "@UnsupportedAppUsage",
    "@VersionSupport",
    "@VintfStability",
    "@nullable",
    "@utf8InCpp",
];

/// Helper that creates a ParseError using the same thread-local source info as SourceGuard.
fn make_parse_error(message: impl Into<String>, start: usize, end: usize) -> AidlError {
    let filename = CURRENT_SOURCE_NAME.with(|name| name.borrow().clone());
    let source = CURRENT_SOURCE_TEXT.with(|text| text.borrow().clone());
    AidlError::from(ParseError {
        src: miette::NamedSource::new(filename, source),
        span: miette::SourceSpan::new(start.into(), end - start),
        message: message.into(),
        help: None,
    })
}

/// Context struct holding the filename and source text of a file to be parsed.
/// Passed to `parse_document()` so that file information is included in error diagnostics.
#[derive(Debug, Clone)]
pub struct SourceContext {
    pub filename: String,
    pub source: String,
}

impl SourceContext {
    pub fn new(filename: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            filename: filename.into(),
            source: source.into(),
        }
    }
}

/// RAII guard: sets the thread-local source context on creation and clears it
/// automatically on drop, ensuring cleanup on both error-return and panic paths.
pub struct SourceGuard;

impl SourceGuard {
    pub fn new(filename: &str, source: &str) -> Self {
        CURRENT_SOURCE_NAME.with(|name| *name.borrow_mut() = filename.to_string());
        CURRENT_SOURCE_TEXT.with(|text| *text.borrow_mut() = source.to_string());
        CURRENT_COMMENTS.with(|spans| *spans.borrow_mut() = scan_comments(source));
        SourceGuard
    }
}

impl Drop for SourceGuard {
    fn drop(&mut self) {
        CURRENT_SOURCE_NAME.with(|name| name.borrow_mut().clear());
        CURRENT_SOURCE_TEXT.with(|text| text.borrow_mut().clear());
        CURRENT_COMMENTS.with(|spans| spans.borrow_mut().clear());
    }
}

/// Returns the filename of the currently active source context.
pub fn current_source_name() -> String {
    CURRENT_SOURCE_NAME.with(|name| name.borrow().clone())
}

/// Returns the source text of the currently active source context.
pub fn current_source_text() -> String {
    CURRENT_SOURCE_TEXT.with(|text| text.borrow().clone())
}

/// One comment in the source, as [`scan_comments`] found it.
#[derive(Debug, Clone, Copy)]
struct CommentSpan {
    start: usize,
    end: usize,
    /// AOSP reads javadoc tags from block comments only; a trailing `//` detaches the run.
    is_block: bool,
}

thread_local! {
    // Current source's comments by start offset; built once per `SourceGuard`, not per decl.
    static CURRENT_COMMENTS: RefCell<Vec<CommentSpan>> = const { RefCell::new(Vec::new()) };
}

/// Every comment in `source`, skipping string/char literals (both admit `\` escapes).
fn scan_comments(source: &str) -> Vec<CommentSpan> {
    let bytes = source.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let start = i;
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                spans.push(CommentSpan {
                    start,
                    end: i,
                    is_block: false,
                });
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let start = i;
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                // Unterminated: runs to end of input; the parser rejects the file anyway.
                i = (i + 2).min(bytes.len());
                spans.push(CommentSpan {
                    start,
                    end: i,
                    is_block: true,
                });
            }
            quote @ (b'"' | b'\'') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    // A backslash escapes the next byte, `\"` included.
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(bytes.len());
            }
            _ => i += 1,
        }
    }
    spans
}

/// The `@deprecated` note attached to the item starting at byte offset
/// `start`, or `None` when it is not deprecated. `Some("")` is a bare
/// `@deprecated` with no note.
///
/// Mirrors AOSP `comments.cpp`: only the **last** comment of the run
/// immediately preceding the item counts, and only when it is a block
/// comment — so a trailing `//` line detaches the javadoc above it, exactly
/// as in AOSP. `start` must be the item's first character *including* its
/// annotations, since the comment precedes those.
pub fn deprecated_at(start: usize) -> Option<String> {
    let span = CURRENT_COMMENTS.with(|spans| {
        let spans = spans.borrow();
        let idx = spans.partition_point(|c| c.end <= start);
        idx.checked_sub(1).map(|i| spans[i])
    })?;
    if !span.is_block {
        return None;
    }
    CURRENT_SOURCE_TEXT.with(|text| {
        let text = text.borrow();
        // Anything but whitespace in between means the comment belongs to an earlier item.
        let gap = text.get(span.end..start)?;
        if !gap.chars().all(char::is_whitespace) {
            return None;
        }
        find_deprecated(text.get(span.start..span.end)?)
    })
}

/// AOSP `TrimmedLines` for a block comment.
fn trimmed_block_lines(body: &str) -> Vec<&str> {
    let stripped = body.strip_prefix("/*").unwrap_or(body);
    let stripped = stripped.strip_suffix("*/").unwrap_or(stripped);
    stripped
        .split('\n')
        .map(|line| {
            let rest = line.trim_start();
            let rest = rest.strip_prefix('*').unwrap_or(rest);
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            rest.trim_end()
        })
        .collect()
}

/// AOSP `BlockTags` + `FindDeprecated`: an `@` line opens a tag; the first `@deprecated` wins.
fn find_deprecated(body: &str) -> Option<String> {
    let mut tag: Option<&str> = None;
    let mut paragraph: Vec<&str> = Vec::new();

    for line in trimmed_block_lines(body) {
        let line = line.trim_start();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('@') {
            if tag == Some("deprecated") {
                return Some(paragraph.join(" "));
            }
            let name_len = rest
                .char_indices()
                .find(|(_, c)| !c.is_ascii_alphabetic())
                .map_or(rest.len(), |(i, _)| i);
            let (name, after) = rest.split_at(name_len);
            tag = Some(name);
            paragraph.clear();
            let after = after.strip_prefix(' ').unwrap_or(after);
            if !after.is_empty() {
                paragraph.push(after);
            }
        } else if tag.is_some() {
            paragraph.push(line);
        }
    }

    (tag == Some("deprecated")).then(|| paragraph.join(" "))
}

pub struct NamespaceGuard();

impl NamespaceGuard {
    pub fn new(ns: &Namespace) -> Self {
        NAMESPACE_STACK.with(|vec| {
            vec.borrow_mut().push(ns.clone());
        });
        Self()
    }
}

impl Drop for NamespaceGuard {
    fn drop(&mut self) {
        NAMESPACE_STACK.with(|vec| {
            vec.borrow_mut().pop();
        });
    }
}

pub fn current_namespace() -> Namespace {
    NAMESPACE_STACK.with(|stack| stack.borrow().last().cloned().unwrap_or_default())
}

fn reset_enum_resolution_state() {
    ENUM_VALUE_CACHE.with(|cache| {
        cache.borrow_mut().clear();
    });
    ENUM_RESOLUTION_STACK.with(|stack| {
        stack.borrow_mut().clear();
    });
}

pub fn set_current_document(document: &Document) {
    let context = DocumentContext::from_document(document);
    set_current_document_context(&context);
}

fn set_current_document_context(context: &DocumentContext) {
    DOCUMENT.with(|doc| {
        let mut doc = doc.borrow_mut();

        doc.package = context.package.clone();
        doc.imports = context.imports.clone();
    })
}

fn current_document_context() -> DocumentContext {
    DOCUMENT.with(|doc| {
        let doc = doc.borrow();
        DocumentContext::from_document(&doc)
    })
}

struct DocumentGuard(DocumentContext);

impl DocumentGuard {
    fn new(context: &DocumentContext) -> Self {
        let previous = current_document_context();
        set_current_document_context(context);
        Self(previous)
    }
}

impl Drop for DocumentGuard {
    fn drop(&mut self) {
        set_current_document_context(&self.0);
    }
}

fn declaration_document_context(ns: &Namespace) -> Option<DocumentContext> {
    DECLARATION_DOCUMENT_MAP.with(|hashmap| hashmap.borrow().get(ns).cloned())
}

fn make_ns_candidate(ns: &Namespace, name: &Namespace) -> Vec<Namespace> {
    let mut res = Vec::new();

    let mut curr_ns = ns.clone();
    curr_ns.push_ns(name);
    res.push(curr_ns.clone());

    if name.ns.len() > 1 {
        curr_ns.pop(); // Remove the last name in case of IntEnum.Foo. Removed the Foo.
        res.push(curr_ns);
    }

    res
}

#[derive(Debug)]
pub struct LookupDecl {
    pub decl: Declaration,
    pub ns: Namespace,
    pub name: Namespace,
}

pub fn lookup_decl_from_name(name: &str, style: &str) -> Option<LookupDecl> {
    let mut namespace = Namespace::new(name, style);

    let mut ns_vec = Vec::new();

    // AOSP `AidlScope::ResolveName` order: enclosing scopes, imports, then the package.
    let package_ns = DOCUMENT.with(|curr_doc| {
        curr_doc
            .borrow()
            .package
            .as_ref()
            .map(|package| Namespace::new(package, Namespace::AIDL))
    });

    // 1. Enclosing scopes outward, unbounded as AOSP `GetEnclosingScope()`; stop at the package.
    let mut curr_ns = current_namespace();
    loop {
        if package_ns.as_ref() == Some(&curr_ns) {
            break;
        }
        ns_vec.append(&mut make_ns_candidate(&curr_ns, &namespace));
        if curr_ns.pop().is_none() {
            break;
        }
    }

    // 2. imports, then the package.
    DOCUMENT.with(|curr_doc| {
        let curr_doc = curr_doc.borrow();
        if let Some(imported) = curr_doc.imports.get(&namespace.ns[0]) {
            let mut new_ns = Namespace::new(imported, Namespace::AIDL);
            new_ns.ns.extend_from_slice(&namespace.ns[1..]);
            ns_vec.push(new_ns.clone());
            // Same shape as the other scopes: `IFoo.BAR` also tries the owner `a.IFoo`.
            if namespace.ns.len() > 1 {
                new_ns.pop();
                ns_vec.push(new_ns);
            }
        }
    });
    if let Some(package_ns) = &package_ns {
        ns_vec.append(&mut make_ns_candidate(package_ns, &namespace));
    }

    // 3. check fully-qualified names as written.
    if namespace.ns.len() > 1 {
        ns_vec.append(&mut make_ns_candidate(&Namespace::default(), &namespace));
    }

    let (decl, ns) = DECLARATION_MAP.with(|hashmap| {
        for ns in &ns_vec {
            if let Some(decl) = hashmap.borrow().get(ns) {
                return Some((decl.clone(), ns.clone()));
            }
        }

        // Only a simple name may fall back to the current decl; `foo.Missing.X` must not.
        if namespace.ns.len() == 1 {
            let curr_ns = current_namespace();
            if let Some(decl) = hashmap.borrow().get(&curr_ns) {
                return Some((decl.clone(), curr_ns));
            }
        }

        None
    })?;

    // A union `Tag` sits in `mod <Union>`: use the union's ns (`<Union>::Tag`, not `Tag::Tag`).
    let effective_ns = match &decl {
        Declaration::Enum(e) if e.tag_of_union.is_some() => {
            e.tag_of_union.clone().expect("checked Some above")
        }
        _ => ns,
    };

    // leave max 2 items because the other items are for name space.
    if namespace.ns.len() > 2 {
        namespace.ns.drain(0..namespace.ns.len() - 2);
    }

    Some(LookupDecl {
        decl,
        ns: effective_ns,
        name: namespace,
    })
}

// Edge of the `declaration_reaches` sizing graph; a `Vec`/`HashMap` is a handle, never an edge.
fn by_value_type_name(ty: &Type) -> Option<&str> {
    if ty.array_types.iter().any(|a| a.const_expr.is_none()) {
        return None;
    }
    // Only `List`/`Map` hold their argument on the heap; a user-defined generic is inline.
    if ty.non_array_type.generic.is_some()
        && matches!(ty.non_array_type.name.as_str(), "List" | "Map")
    {
        return None;
    }
    Some(&ty.non_array_type.name)
}

fn value_members(ns: &Namespace) -> Option<Vec<Declaration>> {
    DECLARATION_MAP.with(|map| match map.borrow().get(ns) {
        Some(Declaration::Parcelable(p)) => Some(p.members.clone()),
        Some(Declaration::Union(u)) => Some(u.members.clone()),
        _ => None,
    })
}

/// Is the declaration at `ns` `@VintfStability`, directly or by inheritance
/// from an enclosing declaration?
///
/// `@VintfStability` is a **scoped** annotation in AOSP: `GetScopedAnnotation`
/// (`aidl_language.cpp`) walks from the type up through its enclosing types,
/// so a nested declaration inside a `@VintfStability` parcelable is itself
/// VINTF-stable without repeating the annotation. That matters on the wire —
/// the stability a parcelable reports is what a `ParcelableHolder` records for
/// it — so the lookup has to walk, not just read the type's own annotations.
///
/// Only declarations nest, so the walk stops at the package boundary of the
/// document that declares `ns` — `DECLARATION_MAP` is keyed by
/// `<package>.<name>`, so without that floor a type whose name matches a
/// package segment (`package a; parcelable b;` beside `package a.b;`) would
/// hand its annotation to every declaration in that package.
pub fn is_vintf_scoped(ns: &Namespace) -> bool {
    let floor = declaration_document_context(ns)
        .and_then(|ctx| ctx.package)
        .map_or(0, |package| {
            Namespace::new(&package, Namespace::AIDL).ns.len()
        });
    let mut ns = ns.clone();
    loop {
        let found = DECLARATION_MAP.with(|map| {
            map.borrow()
                .get(&ns)
                .map(|decl| has_annotation(decl.annotation_list(), AnnotationType::VintfStability))
        });
        match found {
            Some(true) => return true,
            Some(false) => {
                if ns.ns.len() <= floor + 1 || ns.pop().is_none() {
                    return false;
                }
            }
            None => return false,
        }
    }
}

/// Whether `ns` stores members by value, without cloning what [`value_members`] returns.
fn is_value_decl(ns: &Namespace) -> bool {
    DECLARATION_MAP.with(|map| {
        matches!(
            map.borrow().get(ns),
            Some(Declaration::Parcelable(_) | Declaration::Union(_))
        )
    })
}

/// Can `start` reach `target` by following the fields of parcelables and
/// unions? A reference cycle of any length is an infinitely sized Rust type, so
/// the field that closes it has to be boxed — a direct self-reference is only
/// the shortest case.
///
/// Only members held by value are edges, as [`by_value_type_name`] decides.
/// Anything reached through a handle keeps the enclosing type finite and must
/// not be boxed: an interface is a `Strong<dyn …>` — the
/// `CircularParcelable` / `ITestService` pair in the AOSP fixtures is exactly
/// that shape — and a `Vec`/`HashMap` element is behind an allocation.
pub fn declaration_reaches(start: &Namespace, target: &Namespace) -> bool {
    if !is_value_decl(start) || !is_value_decl(target) {
        return false;
    }

    let mut seen = HashSet::new();
    let mut pending = vec![start.clone()];

    while let Some(ns) = pending.pop() {
        if !seen.insert(ns.clone()) {
            continue;
        }
        let Some(members) = value_members(&ns) else {
            continue;
        };

        // Member type names resolve under their declaring namespace and imports.
        let _doc = declaration_document_context(&ns).map(|ctx| DocumentGuard::new(&ctx));
        let _guard = NamespaceGuard::new(&ns);
        for member in &members {
            let Some(var) = member.is_variable() else {
                continue;
            };
            if var.constant {
                continue;
            }
            let Some(name) = by_value_type_name(&var.r#type) else {
                continue;
            };
            let Some(found) = lookup_decl_from_name(name, Namespace::AIDL) else {
                continue;
            };
            // Decide by decl kind, not ns: a union `Tag` reports its union's ns but is a scalar.
            if !matches!(
                found.decl,
                Declaration::Parcelable(_) | Declaration::Union(_)
            ) {
                continue;
            }
            if found.ns == *target {
                return true;
            }
            pending.push(found.ns);
        }
    }
    false
}

thread_local! {
    // `<owner>.<ident>` constants being folded; a true cycle re-enters and bottoms out here.
    static FOLDING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

// Fold in the owner's scope: a raw `BASE + 1` would otherwise pick up the referencer's `BASE`.
fn fold_in_owner_scope(expr: &ConstExpr, owner: &Namespace, ident: &str) -> ConstExpr {
    if *owner == current_namespace() {
        return expr.clone();
    }
    let key = format!("{}.{ident}", owner.to_string(Namespace::AIDL));
    let re_entered = FOLDING.with(|s| s.borrow().contains(&key));
    if re_entered {
        return expr.clone();
    }
    FOLDING.with(|s| s.borrow_mut().push(key));
    let document_context = declaration_document_context(owner);
    let _document_guard = document_context.as_ref().map(DocumentGuard::new);
    let _ns_guard = NamespaceGuard::new(owner);
    let folded = expr.calculate().unwrap_or_else(|_| expr.clone());
    FOLDING.with(|s| {
        s.borrow_mut().pop();
    });
    folded
}

fn make_const_expr(const_expr: Option<&ConstExpr>, lookup_decl: &LookupDecl) -> ConstExpr {
    if let Some(expr) = const_expr {
        let ident = lookup_decl.name.ns.last().map_or("", String::as_str);
        fold_in_owner_scope(expr, &lookup_decl.ns, ident)
    } else {
        let name = if let Some(path) = builtin_rust_path(&lookup_decl.ns) {
            let member = lookup_decl.name.ns.last().map_or("", String::as_str);
            format!("{}::{path}::{member}", type_generator::crate_name())
        } else {
            let ns = current_namespace().relative_mod(&lookup_decl.ns);
            if !ns.is_empty() {
                format!(
                    "{}{}{}",
                    ns,
                    Namespace::RUST,
                    lookup_decl.name.to_string(Namespace::RUST)
                )
            } else {
                lookup_decl.name.to_string(Namespace::RUST)
            }
        };
        ConstExpr::new(ValueType::Name(name))
    }
}

fn lookup_name_from_decl(decl: &Declaration, lookup_decl: &LookupDecl) -> Option<ConstExpr> {
    let lookup_ident = lookup_decl.name.ns.last().unwrap().to_owned();
    match decl {
        Declaration::Variable(decl) => {
            // AOSP resolves a reference against constants only, never a field default.
            if decl.constant && decl.identifier == lookup_ident {
                Some(make_const_expr(decl.const_expr.as_ref(), lookup_decl))
            } else {
                None
            }
        }
        Declaration::Interface(ref decl) => {
            for var in &decl.constant_list {
                if var.identifier == lookup_ident {
                    return Some(make_const_expr(var.const_expr.as_ref(), lookup_decl));
                }
            }
            // `members` holds only nested type declarations; constants live in `constant_list`.
            None
        }

        Declaration::Parcelable(ref decl) => lookup_name_members(&decl.members, lookup_decl),

        Declaration::Enum(ref decl) => {
            for enumerator in &decl.enumerator_list {
                if enumerator.identifier == lookup_ident {
                    return enum_member_const_expr_from_lookup(lookup_decl, &lookup_ident);
                }
            }
            lookup_name_members(&decl.members, lookup_decl)
        }

        Declaration::Union(ref decl) => lookup_name_members(&decl.members, lookup_decl),
    }
}

// Direct members only: `Outer.X` never means `Outer.Inner.X`; a nested owner is its own candidate.
fn lookup_name_members(members: &[Declaration], lookup_decl: &LookupDecl) -> Option<ConstExpr> {
    members
        .iter()
        .filter(|decl| matches!(decl, Declaration::Variable(_)))
        .find_map(|decl| lookup_name_from_decl(decl, lookup_decl))
}

pub(crate) fn enum_member_const_expr_from_lookup(
    lookup_decl: &LookupDecl,
    member_name: &str,
) -> Option<ConstExpr> {
    let Declaration::Enum(enum_decl) = &lookup_decl.decl else {
        return None;
    };

    let mut enum_val: i64 = 0;
    let enum_type = lookup_decl.ns.to_string(Namespace::AIDL);
    let resolution_key = format!("{enum_type}.{member_name}");

    if let Some(cached) =
        ENUM_VALUE_CACHE.with(|cache| cache.borrow().get(&resolution_key).cloned())
    {
        return Some(cached);
    }

    let is_circular = ENUM_RESOLUTION_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        if stack.contains(&resolution_key) {
            true
        } else {
            stack.insert(resolution_key.clone());
            false
        }
    });
    if is_circular {
        return None;
    }

    let document_context = declaration_document_context(&lookup_decl.ns);
    let _document_guard = document_context.as_ref().map(DocumentGuard::new);
    let _guard = NamespaceGuard::new(&lookup_decl.ns);
    let mut result = None;

    // An unfoldable explicit value poisons auto-increment so `decl_enum` diagnoses, not zeroes.
    let mut carried: Option<ConstExpr> = None;
    let mut result_is_carried = false;
    for enumerator in &enum_decl.enumerator_list {
        if let Some(const_expr) = &enumerator.const_expr {
            match const_expr.calculate() {
                Ok(calculated) => match &calculated.value {
                    ValueType::Name(_) => carried = Some(const_expr.clone()),
                    // AOSP `AreCompatibleOperandTypes`: bool is integral; the rest poison.
                    ValueType::Byte(_)
                    | ValueType::Int32(_)
                    | ValueType::Int64(_)
                    | ValueType::Bool(_)
                    | ValueType::Reference { .. } => match calculated.to_i64() {
                        Ok(v) => {
                            enum_val = v;
                            carried = None;
                        }
                        Err(_) => carried = Some(calculated),
                    },
                    _ => carried = Some(calculated),
                },
                Err(_) => carried = Some(const_expr.clone()),
            }
        }

        if enumerator.identifier == member_name {
            match carried.take() {
                Some(expr) => {
                    result = Some(expr);
                    result_is_carried = true;
                }
                None => {
                    result = Some(ConstExpr::new(ValueType::Reference {
                        enum_type: enum_type.clone(),
                        enum_name: enum_decl.name.clone(),
                        member_name: member_name.to_string(),
                        value: enum_val,
                    }))
                }
            }
            break;
        }

        // AOSP auto-increments with `previous + 1`, whose fold rejects an overflow.
        match enum_val.checked_add(1) {
            Some(next) => enum_val = next,
            None => {
                let next = ConstExpr::new_expr(
                    ConstExpr::new(ValueType::Int64(enum_val)),
                    "+",
                    ConstExpr::new(ValueType::Int64(1)),
                );
                carried = carried.or(Some(next));
            }
        }
    }

    ENUM_RESOLUTION_STACK.with(|stack| {
        stack.borrow_mut().remove(&resolution_key);
    });

    // Never cache a carried result: frozen before symbols register, it duplicates discriminants.
    if let Some(expr) = &result {
        if !result_is_carried {
            ENUM_VALUE_CACHE.with(|cache| {
                cache.borrow_mut().insert(resolution_key, expr.clone());
            });
        }
    }

    result
}

pub fn name_to_enum_member_const_expr(name: &str, target_enum: Option<&str>) -> Option<ConstExpr> {
    // A field default's target type resolves bare members and rejects other enums' members.
    if let Some((enum_name, member_name)) = name.rsplit_once('.') {
        let lookup_decl = lookup_decl_from_name(enum_name, Namespace::AIDL)?;
        // The simple-name fallback may return the current declaration under another name.
        let written = enum_name.rsplit('.').next().unwrap_or(enum_name);
        if !matches!(&lookup_decl.decl, Declaration::Enum(e) if e.name == written) {
            return None;
        }

        if let Some(target_enum) = target_enum {
            let target_lookup = lookup_decl_from_name(target_enum, Namespace::AIDL)?;
            if lookup_decl.ns != target_lookup.ns {
                return None;
            }
        }

        return enum_member_const_expr_from_lookup(&lookup_decl, member_name);
    }

    if let Some(target_enum) = target_enum {
        let lookup_decl = lookup_decl_from_name(target_enum, Namespace::AIDL)?;
        if matches!(lookup_decl.decl, Declaration::Enum(_)) {
            return enum_member_const_expr_from_lookup(&lookup_decl, name);
        }
        return None;
    }

    let curr_ns = current_namespace();
    DECLARATION_MAP.with(|hashmap| {
        let lookup_decl = hashmap
            .borrow()
            .get(&curr_ns)
            .filter(|decl| matches!(decl, Declaration::Enum(_)))
            .cloned()
            .map(|decl| LookupDecl {
                decl,
                ns: curr_ns.clone(),
                name: Namespace::new(name, Namespace::AIDL),
            })?;
        enum_member_const_expr_from_lookup(&lookup_decl, name)
    })
}

// AOSP `Parser::CheckValidTypeName`; only an unstructured parcelable may be qualified.
fn reject_qualified_type_name(name: &str, span: &pest::Span<'_>) -> Result<(), AidlError> {
    if !name.contains('.') {
        return Ok(());
    }
    Err(make_parse_error(
        format!("type name '{name}' can't be qualified; use `package`"),
        span.start(),
        span.end(),
    ))
}

// `self`/`Self`/`super`/`crate`/`_` cannot be raw identifiers, so no generated name can carry them.
fn reject_unrepresentable_identifier(
    ident: &str,
    role: &str,
    span: &pest::Span<'_>,
) -> Result<(), AidlError> {
    let Some(keyword) = ident
        .split('.')
        .find(|segment| matches!(*segment, "self" | "Self" | "super" | "crate" | "_"))
    else {
        return Ok(());
    };
    Err(make_parse_error(
        format!(
            "'{keyword}' cannot be used as a{} {role} \
             (not representable as a Rust raw identifier)",
            // `u` is excluded: every role starting with it reads "a" ("a union name").
            if role.starts_with(['a', 'e', 'i', 'o']) {
                "n"
            } else {
                ""
            }
        ),
        span.start(),
        span.end(),
    ))
}

// Promote to `@Backing` (`byte` -> `int`, AOSP `AidlConstantReference`); unknown enum -> i64.
pub(crate) fn enum_reference_promoted(enum_type: &str, value: i64) -> ConstExpr {
    let backing = lookup_decl_from_name(enum_type, crate::Namespace::AIDL).and_then(|lookup| {
        match lookup.decl {
            Declaration::Enum(decl) => get_backing_type(&decl.annotation_list, decl.name_span)
                .ok()
                .map(|generator| generator.value_type),
            _ => None,
        }
    });
    match backing {
        // Too wide is `decl_enum`'s diagnostic (only if generated); never truncate here.
        Some(ValueType::Byte(_)) | Some(ValueType::Int32(_)) => match i32::try_from(value) {
            Ok(v) => ConstExpr::new(ValueType::Int32(v)),
            Err(_) => ConstExpr::new(ValueType::Int64(value)),
        },
        _ => ConstExpr::new(ValueType::Int64(value)),
    }
}

// Universal symbol registration - supports all types of named constants
pub fn register_symbol(name: &str, value: ConstExpr, namespace: Option<&str>) {
    SYMBOL_TABLE.with(|table| {
        let mut table = table.borrow_mut();

        // Key by declaring namespace: a bare key lets an unrelated decl's constant win.
        match namespace {
            Some(ns) => {
                table.insert(format!("{ns}.{name}"), value);
            }
            None => {
                table.insert(name.to_string(), value);
            }
        }
    });
}

// Enhanced name resolution with universal symbol table
pub fn name_to_const_expr(name: &str) -> Option<ConstExpr> {
    if let Some(expr) = name_to_enum_member_const_expr(name, None) {
        return Some(expr);
    }

    // Dotted names: namespace-aware lookup first, since suffix stripping loses the parent type.
    if name.contains('.') {
        if let Some(lookup_decl) = lookup_decl_from_name(name, Namespace::AIDL) {
            if let Some(expr) = lookup_name_from_decl(&lookup_decl.decl, &lookup_decl) {
                return Some(expr);
            }
        }
    }

    // Scope-first order: an unqualified name resolves against its own declaration first.
    let alternative_formats = generate_name_variants(name);
    for variant in alternative_formats {
        let variant_result = SYMBOL_TABLE.with(|table| table.borrow().get(&variant).cloned());
        if let Some(expr) = variant_result {
            // The key is `<owner ns>.<name>`; fold in that owner's scope.
            return Some(match variant.rsplit_once('.') {
                Some((owner, ident)) => {
                    fold_in_owner_scope(&expr, &Namespace::new(owner, Namespace::AIDL), ident)
                }
                None => expr,
            });
        }
    }

    // Fallback to original resolution
    if let Some(lookup_decl) = lookup_decl_from_name(name, Namespace::AIDL) {
        return lookup_name_from_decl(&lookup_decl.decl, &lookup_decl);
    }

    None
}

// Symbol-table keys, most specific first; an unqualified name searches enclosing scopes only.
fn generate_name_variants(name: &str) -> Vec<String> {
    let mut variants = Vec::new();
    let dotted = name.contains('.');

    if dotted {
        variants.push(name.to_string());
    }

    let current = current_namespace();
    // Stop at the package boundary: a package segment is not a scope that holds constants.
    let floor = declaration_document_context(&current)
        .and_then(|ctx| ctx.package)
        .map_or(0, |package| {
            Namespace::new(&package, Namespace::AIDL).ns.len()
        });
    let current_ns = current.to_string(crate::Namespace::AIDL);
    if !current_ns.is_empty() {
        let segments: Vec<&str> = current_ns.split('.').collect();
        for end in (floor + 1..=segments.len()).rev() {
            variants.push(format!("{}.{}", segments[..end].join("."), name));
        }
    }

    if dotted {
        // Progressively shorter suffixes: "A.B.C" -> "B.C", "C".
        let parts: Vec<&str> = name.split('.').collect();
        for i in 1..parts.len() {
            variants.push(parts[i..].join("."));
        }
    } else {
        variants.push(name.to_string());
    }

    variants
}

#[derive(Debug)]
pub struct Document {
    pub package: Option<String>,
    pub imports: HashMap<String, String>,
    pub decls: Vec<Declaration>,
    /// Non-fatal diagnostics produced while parsing this document
    /// (e.g. unknown annotations). [`Builder::generate`](crate::Builder::generate)
    /// emits each as `cargo:warning=<msg>` so it surfaces in cargo
    /// output without aborting the build.
    pub warnings: Vec<crate::error::AidlWarning>,
}

impl Document {
    fn new() -> Self {
        Self {
            package: None,
            imports: HashMap::new(),
            decls: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct DocumentContext {
    package: Option<String>,
    imports: HashMap<String, String>,
}

impl DocumentContext {
    fn from_document(document: &Document) -> Self {
        Self {
            package: document.package.clone(),
            imports: document.imports.clone(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct VariableDecl {
    pub constant: bool,
    pub annotation_list: Vec<Annotation>,
    pub r#type: Type,
    pub identifier: String,
    pub const_expr: Option<ConstExpr>,
    /// `@deprecated` note from the preceding javadoc block, if any. `Some("")`
    /// is a bare tag. See [`deprecated_at`].
    pub deprecated: Option<String>,
}

impl VariableDecl {
    pub fn identifier(&self) -> String {
        self.identifier.to_owned()
    }

    /// Constant names are emitted verbatim, matching AOSP's Rust backend —
    /// any renaming would collide distinct-case constants (`foo` / `FOO`).
    pub fn const_identifier(&self) -> String {
        self.identifier.to_owned()
    }

    pub fn union_identifier(&self) -> String {
        self.identifier.to_case(Case::UpperCamel)
    }

    pub fn member_init(&self) -> String {
        "Default::default()".into()
    }
}

#[derive(Debug, Default, Clone)]
pub struct InterfaceDecl {
    pub namespace: Namespace,
    pub annotation_list: Vec<Annotation>,
    pub oneway: bool,
    pub name: String,
    pub name_span: Option<(usize, usize)>,
    pub method_list: Vec<MethodDecl>,
    pub constant_list: Vec<VariableDecl>,
    pub members: Vec<Declaration>,
    pub deprecated: Option<String>,
}

impl InterfaceDecl {
    pub fn pre_process(&mut self) {
        for decl in &mut self.constant_list {
            decl.const_expr = decl
                .const_expr
                .as_ref()
                .map(|expr| expr.calculate().unwrap_or_else(|_| expr.clone()));
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ParcelableDecl {
    pub annotation_list: Vec<Annotation>,
    pub namespace: Namespace,
    pub name: String,
    pub name_span: Option<(usize, usize)>,
    pub type_params: Vec<TypeParam>,
    pub cpp_header: String,
    pub ndk_header: String,
    pub rust_type: String,
    /// Declared with `;` instead of a body: the definition lives outside AIDL.
    pub is_unstructured: bool,
    pub members: Vec<Declaration>,
    pub deprecated: Option<String>,
}

impl ParcelableDecl {
    pub fn pre_process(&mut self) {
        for decl in &mut self.members {
            if let Declaration::Variable(decl) = decl {
                decl.const_expr = decl
                    .const_expr
                    .as_ref()
                    .map(|expr| expr.calculate().unwrap_or_else(|_| expr.clone()));
            }
        }
    }
}

#[derive(Debug, Default, Clone)]
pub enum Direction {
    #[default]
    None,
    In,
    Out,
    Inout,
}

#[derive(Debug, Default, Clone)]
pub struct Arg {
    pub direction: Direction,
    pub direction_span: Option<(usize, usize)>,
    pub r#type: Type,
    pub identifier: String,
}

impl Arg {
    pub fn to_generator(&self) -> Result<type_generator::TypeGenerator, crate::error::AidlError> {
        let generator = type_generator::TypeGenerator::new_with_type(&self.r#type)?;

        Ok(generator
            .direction_at(&self.direction, self.direction_span)?
            .identifier(&self.identifier))
    }

    pub fn is_mutable(&self) -> bool {
        matches!(self.direction, Direction::Inout | Direction::Out)
    }
}

#[derive(Debug, Default, Clone)]
pub struct MethodDecl {
    pub annotation_list: Vec<Annotation>,
    pub oneway: bool,
    pub r#type: Type,
    pub identifier: String,
    pub identifier_span: Option<(usize, usize)>,
    pub arg_list: Vec<Arg>,
    pub intvalue: Option<i64>,
    pub intvalue_span: Option<(usize, usize)>,
    pub deprecated: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Declaration {
    Parcelable(ParcelableDecl),
    Interface(InterfaceDecl),
    Enum(EnumDecl),
    Union(UnionDecl),
    Variable(VariableDecl),
}

impl Declaration {
    pub fn is_variable(&self) -> Option<&VariableDecl> {
        if let Declaration::Variable(decl) = self {
            Some(decl)
        } else {
            None
        }
    }

    pub fn namespace(&self) -> &Namespace {
        match self {
            Declaration::Parcelable(decl) => &decl.namespace,
            Declaration::Interface(decl) => &decl.namespace,
            Declaration::Enum(decl) => &decl.namespace,
            Declaration::Union(decl) => &decl.namespace,
            _ => unreachable!(),
        }
    }

    pub fn set_namespace(&mut self, namespace: Namespace) {
        match self {
            Declaration::Parcelable(decl) => decl.namespace = namespace,
            Declaration::Interface(decl) => decl.namespace = namespace,
            Declaration::Enum(decl) => decl.namespace = namespace,
            Declaration::Union(decl) => decl.namespace = namespace,
            _ => unreachable!(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Declaration::Parcelable(decl) => &decl.name,
            Declaration::Interface(decl) => &decl.name,
            Declaration::Enum(decl) => &decl.name,
            Declaration::Union(decl) => &decl.name,
            _ => unreachable!(),
        }
    }

    /// Records the `@deprecated` note resolved from the declaration's leading
    /// javadoc. Set by the parse sites, which are the only place the
    /// declaration's own start offset is still in hand.
    pub fn set_deprecated(&mut self, deprecated: Option<String>) {
        match self {
            Declaration::Parcelable(decl) => decl.deprecated = deprecated,
            Declaration::Interface(decl) => decl.deprecated = deprecated,
            Declaration::Enum(decl) => decl.deprecated = deprecated,
            Declaration::Union(decl) => decl.deprecated = deprecated,
            Declaration::Variable(decl) => decl.deprecated = deprecated,
        }
    }

    pub fn annotation_list(&self) -> &[Annotation] {
        match self {
            Declaration::Parcelable(decl) => &decl.annotation_list,
            Declaration::Interface(decl) => &decl.annotation_list,
            Declaration::Enum(decl) => &decl.annotation_list,
            Declaration::Union(decl) => &decl.annotation_list,
            Declaration::Variable(decl) => &decl.annotation_list,
        }
    }

    pub fn members_mut(&mut self) -> &mut Vec<Declaration> {
        match self {
            Declaration::Parcelable(decl) => &mut decl.members,
            Declaration::Interface(decl) => &mut decl.members,
            Declaration::Enum(decl) => &mut decl.members,
            Declaration::Union(decl) => &mut decl.members,
            _ => unreachable!(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Parameter {
    identifier: String,
    const_expr: ConstExpr,
}

#[derive(Debug, Default, Clone)]
pub struct Annotation {
    pub annotation: String,
    pub const_expr: Option<ConstExpr>,
    pub parameter_list: Vec<Parameter>,
    pub annotation_span: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub enum Generic {
    Type1 {
        type_args1: Vec<Type>,
        non_array_type: NonArrayType,
        type_args2: Vec<Type>,
    },
    Type2 {
        non_array_type: NonArrayType,
        type_args: Vec<Type>,
    },
    Type3 {
        type_args: Vec<Type>,
    },
}

impl Generic {
    /// The type arguments in order; shapes 1 and 2 yield their inner `Bar<X>` as one argument.
    pub fn type_args(&self) -> Vec<Type> {
        let nested = |non_array_type: &NonArrayType, inner: &[Type]| Type {
            annotation_list: Vec::new(),
            non_array_type: NonArrayType {
                name: non_array_type.name.clone(),
                generic: Some(Box::new(Generic::Type3 {
                    type_args: inner.to_vec(),
                })),
                name_span: non_array_type.name_span,
            },
            array_types: Vec::new(),
        };
        match self {
            Generic::Type1 {
                type_args1,
                non_array_type,
                type_args2,
            } => {
                let mut args = type_args1.clone();
                args.push(nested(non_array_type, type_args2));
                args
            }
            Generic::Type2 {
                non_array_type,
                type_args,
            } => vec![nested(non_array_type, type_args)],
            Generic::Type3 { type_args } => type_args.clone(),
        }
    }

    /// The first type argument's value type; panics on a `Generic` with no arguments.
    pub fn to_value_type(&self) -> Result<ValueType, crate::error::AidlError> {
        let args = self.type_args();
        Ok(type_generator::TypeGenerator::new_with_type(&args[0])?.value_type)
    }
}

#[derive(Debug, Default, Clone)]
pub struct NonArrayType {
    pub name: String,
    pub generic: Option<Box<Generic>>,
    pub name_span: Option<(usize, usize)>,
}

#[derive(Debug, Default, Clone)]
pub struct ArrayType {
    pub const_expr: Option<ConstExpr>,
}

#[derive(Debug, Default, Clone)]
pub struct Type {
    pub annotation_list: Vec<Annotation>,
    pub non_array_type: NonArrayType,
    pub array_types: Vec<ArrayType>,
}

impl Type {
    pub fn to_generator(&self) -> Result<type_generator::TypeGenerator, crate::error::AidlError> {
        type_generator::TypeGenerator::new_with_type(self)
    }
}

#[derive(PartialEq)]
pub enum AnnotationType {
    IsNullable,
    /// `@JavaOnlyStableParcelable` — the parcelable is declared outside AIDL
    /// for the Java backend, so no Rust definition can be generated. Matched
    /// exactly: `@JavaOnlyImmutable` is a *structured* parcelable that AOSP's
    /// Rust backend generates normally.
    JavaOnlyStableParcelable,
    VintfStability,
    /// `@FixedSize` — every field must itself be fixed size. Unlike
    /// `@VintfStability` this is **not** scoped: AOSP reads it with the plain
    /// `GetAnnotation`, so a nested declaration does not inherit it.
    FixedSize,
}

/// Returns whether the annotation list contains the queried annotation.
pub fn has_annotation(annotation_list: &[Annotation], query_type: AnnotationType) -> bool {
    annotation_list.iter().any(|annotation| match query_type {
        AnnotationType::VintfStability => annotation.annotation == "@VintfStability",
        AnnotationType::IsNullable => annotation.annotation == "@nullable",
        AnnotationType::JavaOnlyStableParcelable => {
            annotation.annotation == "@JavaOnlyStableParcelable"
        }
        AnnotationType::FixedSize => annotation.annotation == "@FixedSize",
    })
}

/// The traits a `@RustDerive(...)` parameter may name.
const RUST_DERIVE_SCHEMA: &[&str] = &[
    "Copy",
    "Clone",
    "PartialOrd",
    "Ord",
    "PartialEq",
    "Eq",
    "Hash",
];

/// Always emitted by the templates; `@RustDerive` accepts and drops them to avoid a double derive.
const RUST_DERIVE_ALWAYS_EMITTED: &[&str] = &["Debug", "Default"];

/// Collects the enabled `@RustDerive(...)` trait names as a comma-separated
/// list (e.g. `"Clone,PartialEq"`), or an empty string when the annotation is
/// absent. The result is interpolated directly into the generated `#[derive]`.
pub fn rust_derive_list(annotation_list: &[Annotation]) -> String {
    for annotation in annotation_list {
        if annotation.annotation == "@RustDerive" {
            return annotation
                .parameter_list
                .iter()
                .filter(|param| param.const_expr.to_bool().unwrap_or(false))
                .filter(|param| !RUST_DERIVE_ALWAYS_EMITTED.contains(&param.identifier.as_str()))
                .map(|param| param.identifier.to_owned())
                .collect::<Vec<_>>()
                .join(",");
        }
    }
    String::new()
}

/// Parsed AOSP `@EnforcePermission` annotation. Mirrors the three forms
/// `aidl_language.cpp::AidlAnnotation::EnforceExpression()` accepts:
/// `@EnforcePermission("X")` / `@EnforcePermission(value = "X")` =
/// [`EnforcePermissionExpr::Single`], `@EnforcePermission(allOf = {...})`
/// = [`EnforcePermissionExpr::AllOf`], `@EnforcePermission(anyOf = {...})`
/// = [`EnforcePermissionExpr::AnyOf`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnforcePermissionExpr {
    Single(String),
    AllOf(Vec<String>),
    AnyOf(Vec<String>),
}

/// The first constant name an unevaluated annotation value refers to.
fn const_reference(expr: &ConstExpr) -> Option<&str> {
    match &expr.value {
        ValueType::Name(name) => Some(name),
        ValueType::Array(items) => items.iter().find_map(const_reference),
        ValueType::Map(key, value) => const_reference(key).or_else(|| const_reference(value)),
        ValueType::Expr { lhs, rhs, .. } => const_reference(lhs).or_else(|| const_reference(rhs)),
        ValueType::Unary { expr, .. } => const_reference(expr),
        _ => None,
    }
}

/// The folded string value (AOSP `ParamValue<std::string>`); the grammar stripped the quotes.
fn const_expr_as_string(expr: &ConstExpr) -> Option<String> {
    if let Ok(ValueType::String(s)) = expr.calculate().map(|c| c.value) {
        Some(s)
    } else {
        None
    }
}

/// A string array's folded items, or `None` (AOSP `AidlAnnotation::CheckValid()` rejects).
fn const_expr_as_string_array(expr: &ConstExpr) -> Option<Vec<String>> {
    let Ok(ValueType::Array(items)) = expr.calculate().map(|c| c.value) else {
        return None;
    };
    let mut out = Vec::with_capacity(items.len());
    for item in &items {
        out.push(const_expr_as_string(item)?);
    }
    Some(out)
}

/// Extracts a parsed `@EnforcePermission(...)` from an annotation list:
/// `Ok(None)` when the annotation is absent;
/// `Err(MalformedEnforcePermission)` when it is present with no parameter,
/// or with a parameter that is not one of `value`/`allOf`/`anyOf` or whose
/// value has the wrong type (AOSP `AidlAnnotation::CheckValid()` fails the
/// build too).
///
/// AOSP schema reference: `aidl_language.cpp:211-214` declares
/// `EnforcePermission` with `{{"value", kStringType}, {"anyOf",
/// kStringArrayType}, {"allOf", kStringArrayType}}` — exactly the three
/// forms decoded here. When several are given, the expression is chosen as
/// `AidlAnnotation::EnforceExpression()` does, regardless of source order:
/// `value`, else `anyOf`, else `allOf`.
pub fn enforce_permission_from_annotation_list(
    annotation_list: &[Annotation],
    method_name: &str,
) -> Result<Option<EnforcePermissionExpr>, crate::error::AidlError> {
    for annotation in annotation_list {
        if annotation.annotation != "@EnforcePermission" {
            continue;
        }

        // Shorthand `@EnforcePermission("X")`: the positional argument is in `const_expr`.
        if let Some(c) = &annotation.const_expr {
            if let Some(s) = const_expr_as_string(c) {
                return Ok(Some(EnforcePermissionExpr::Single(s)));
            }
        }

        // Named-parameter forms: AOSP `CheckValid()` rejects any unknown or ill-typed parameter.
        let (mut single, mut any_of, mut all_of) = (None, None, None);
        let mut well_formed = true;
        for param in &annotation.parameter_list {
            let ok = match param.identifier.as_str() {
                "value" => const_expr_as_string(&param.const_expr).map(|s| single = Some(s)),
                "anyOf" => const_expr_as_string_array(&param.const_expr).map(|v| any_of = Some(v)),
                "allOf" => const_expr_as_string_array(&param.const_expr).map(|v| all_of = Some(v)),
                _ => None,
            };
            well_formed &= ok.is_some();
        }

        // AOSP `EnforceExpression()` order, independent of source order: value, anyOf, allOf.
        if well_formed {
            if let Some(s) = single {
                return Ok(Some(EnforcePermissionExpr::Single(s)));
            }
            if let Some(items) = any_of {
                return Ok(Some(EnforcePermissionExpr::AnyOf(items)));
            }
            if let Some(items) = all_of {
                return Ok(Some(EnforcePermissionExpr::AllOf(items)));
            }
        }

        // Present but malformed: refuse an unguarded Bn, as AOSP fails the build.
        let (start, end) = annotation.annotation_span.unwrap_or((0, 0));
        return Err(crate::error::AidlError::Semantic(Box::new(
            crate::error::SemanticError::MalformedEnforcePermission {
                method: method_name.to_string(),
                src: miette::NamedSource::new(current_source_name(), current_source_text()),
                span: (start, end.saturating_sub(start)).into(),
            },
        )));
    }
    Ok(None)
}

/// AOSP `AidlInterface::Version`: `@VersionSupport` wins; `--version` applies only without it.
pub(crate) fn interface_version(
    annotation_list: &[Annotation],
    cli_version: Option<i32>,
) -> Result<Option<i32>, AidlError> {
    let Some(annotation) = annotation_list
        .iter()
        .find(|a| a.annotation == "@VersionSupport")
    else {
        return Ok(cli_version);
    };
    let fail = |message: String| make_invalid_operation_error(message, annotation.annotation_span);
    // Schema `{"version", kIntType, required}`, as AOSP `AidlAnnotation::CheckValid`.
    let mut expr = None;
    for param in &annotation.parameter_list {
        if param.identifier != "version" {
            return Err(fail(format!(
                "Parameter {} not supported for annotation VersionSupport.",
                param.identifier
            )));
        }
        expr = Some(&param.const_expr);
    }
    let expr = expr.ok_or_else(|| fail("Missing 'version' on @VersionSupport.".into()))?;
    let version = match expr.calculate().map(|c| c.value) {
        Ok(ValueType::Byte(v)) => Some(i32::from(v)),
        Ok(ValueType::Int32(v)) => Some(v),
        Ok(ValueType::Int64(v)) => i32::try_from(v).ok(),
        _ => None,
    }
    .ok_or_else(|| {
        fail("Invalid value for parameter version on annotation VersionSupport.".into())
    })?;
    // AOSP `VersionSpecificCheckValid`.
    if let Some(cli) = cli_version.filter(|&cli| cli != version) {
        return Err(fail(format!(
            "The version declared in the @VersionSupport version variable ({version}) must match \
             the actual version of the interface ({cli})."
        )));
    }
    Ok(Some(version))
}

/// The folded `@Descriptor` value; schema `{"value", kStringType, required}` as AOSP `CheckValid`.
pub fn get_descriptor_from_annotation_list(
    annotation_list: &[Annotation],
) -> Result<Option<String>, AidlError> {
    let Some(annotation) = annotation_list
        .iter()
        .find(|a| a.annotation == "@Descriptor")
    else {
        return Ok(None);
    };
    let fail = |message: String| make_invalid_operation_error(message, annotation.annotation_span);
    let mut expr = None;
    for param in &annotation.parameter_list {
        if param.identifier != "value" {
            return Err(fail(format!(
                "Parameter {} not supported for annotation Descriptor.",
                param.identifier
            )));
        }
        expr = Some(&param.const_expr);
    }
    let expr = expr.ok_or_else(|| fail("Missing 'value' on @Descriptor.".into()))?;
    let value = const_expr_as_string(expr).ok_or_else(|| {
        fail("Invalid value for parameter value on annotation Descriptor.".into())
    })?;
    // AOSP `AidlInterface::GetDescriptor`: an empty override falls back to the canonical name.
    Ok((!value.is_empty()).then_some(value))
}

pub fn get_backing_type(
    annotation_list: &Vec<Annotation>,
    name_span: Option<(usize, usize)>,
) -> Result<type_generator::TypeGenerator, crate::error::AidlError> {
    // parse "@Backing(type="byte")"
    for annotation in annotation_list {
        if annotation.annotation == "@Backing" {
            // AOSP schema `{"type", kStringType, required}`: a missing `type` is no byte default.
            let span = annotation.annotation_span.or(name_span);
            if let Some(param) = annotation
                .parameter_list
                .iter()
                .find(|p| p.identifier != "type")
            {
                return Err(make_invalid_operation_error(
                    format!(
                        "Parameter {} not supported for annotation Backing.",
                        param.identifier
                    ),
                    span,
                ));
            }
            for param in &annotation.parameter_list {
                if param.identifier == "type" {
                    let type_name: String =
                        param.const_expr.to_value_string().trim_matches('"').into();

                    // AOSP `AidlEnumDeclaration::Autofill()`: only byte, int, long.
                    if !matches!(type_name.as_str(), "byte" | "int" | "long") {
                        return Err(make_invalid_backing_type_error(
                            type_name,
                            annotation.annotation_span.or(name_span),
                        ));
                    }

                    return type_generator::TypeGenerator::new(&NonArrayType {
                        name: type_name,
                        generic: None,
                        name_span,
                    });
                }
            }
            return Err(make_invalid_operation_error(
                "Missing 'type' on @Backing.".into(),
                span,
            ));
        }
    }

    type_generator::TypeGenerator::new(&NonArrayType {
        // The cstr is enclosed in quotes.
        name: "byte".into(),
        generic: None,
        name_span: None,
    })
}

/// Builds an `InvalidBackingType` diagnostic from the active source context.
fn make_invalid_backing_type_error(type_name: String, span: Option<(usize, usize)>) -> AidlError {
    let filename = CURRENT_SOURCE_NAME.with(|name| name.borrow().clone());
    let source = CURRENT_SOURCE_TEXT.with(|text| text.borrow().clone());
    let (start, end) = span.unwrap_or((0, 0));
    AidlError::from(crate::error::SemanticError::InvalidBackingType {
        type_name,
        src: miette::NamedSource::new(filename, source),
        span: miette::SourceSpan::new(start.into(), end.saturating_sub(start)),
    })
}

/// Builds an `InvalidOperation` diagnostic from the active source context.
pub(crate) fn make_invalid_operation_error(
    message: String,
    span: Option<(usize, usize)>,
) -> AidlError {
    let filename = CURRENT_SOURCE_NAME.with(|name| name.borrow().clone());
    let source = CURRENT_SOURCE_TEXT.with(|text| text.borrow().clone());
    let (start, end) = span.unwrap_or((0, 0));
    AidlError::from(crate::error::SemanticError::InvalidOperation {
        message,
        src: miette::NamedSource::new(filename, source),
        span: miette::SourceSpan::new(start.into(), end.saturating_sub(start)),
    })
}

/// Whether a method's return type is the AIDL primitive `void`.
fn is_void_return(ty: &Type) -> bool {
    ty.array_types.is_empty() && ty.non_array_type.name == "void"
}

/// AOSP `aidl_language.cpp:1211`: oneway (own or interface-wide) bars returns and `out`/`inout`.
fn validate_oneway_methods(interface: &InterfaceDecl) -> Result<(), AidlError> {
    let mut errors = Vec::new();
    for method in &interface.method_list {
        let is_oneway = interface.oneway || method.oneway;
        if !is_oneway {
            continue;
        }
        if !is_void_return(&method.r#type) {
            errors.push(make_invalid_operation_error(
                format!(
                    "oneway method '{}' cannot return a value",
                    method.identifier
                ),
                method.identifier_span,
            ));
        }
        for arg in &method.arg_list {
            let dir = match arg.direction {
                Direction::Out => "out",
                Direction::Inout => "inout",
                _ => continue,
            };
            errors.push(make_invalid_operation_error(
                format!(
                    "oneway method '{}' cannot have an '{}' parameter",
                    method.identifier, dir
                ),
                arg.direction_span,
            ));
        }
    }
    match AidlError::collect(errors) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn parse_unary(mut pairs: pest::iterators::Pairs<Rule>) -> Result<ConstExpr, AidlError> {
    let operator = pairs.next().unwrap().as_str().to_owned();
    let factor = parse_factor(pairs.next().unwrap().into_inner().next().unwrap())?;
    Ok(ConstExpr::new_unary(&operator, factor))
}

fn parse_intvalue(arg_value: &str, span: (usize, usize)) -> Result<ConstExpr, AidlError> {
    let mut is_u8 = false;
    let mut is_long = false;

    let (value, radix) = if arg_value.starts_with("0x") || arg_value.starts_with("0X") {
        (&arg_value[2..], 16)
    } else {
        (arg_value, 10)
    };

    // AOSP suffixes u8/u32/u64/l/L; the multi-char ones are checked before `l`/`L`.
    let mut is_u32 = false;
    let mut is_u64 = false;
    let value = if let Some(stripped) = value.strip_suffix("u64") {
        is_u64 = true;
        stripped
    } else if let Some(stripped) = value.strip_suffix("u32") {
        is_u32 = true;
        stripped
    } else if let Some(stripped) = value.strip_suffix("u8") {
        is_u8 = true;
        stripped
    } else if value.ends_with('l') || value.ends_with('L') {
        is_long = true;
        &value[..value.len() - 1]
    } else {
        value
    };

    // AOSP allows `_` separators (`0xFF_FF`); `from_str_radix` does not, so strip them.
    let cleaned;
    let value: &str = if value.contains('_') {
        cleaned = value.replace('_', "");
        &cleaned
    } else {
        value
    };

    // Explicit u32 / u64 suffixes pin the target size regardless of radix.
    if is_u32 {
        let parsed_value = u32::from_str_radix(value, radix).map_err(|err| {
            make_parse_error(
                format!("invalid u32 literal '{arg_value}': {err}"),
                span.0,
                span.1,
            )
        })?;
        return Ok(ConstExpr::new(ValueType::Int32(parsed_value as i32 as _)));
    }
    if is_u64 {
        let parsed_value = u64::from_str_radix(value, radix).map_err(|err| {
            make_parse_error(
                format!("invalid u64 literal '{arg_value}': {err}"),
                span.0,
                span.1,
            )
        })?;
        return Ok(ConstExpr::new(ValueType::Int64(parsed_value as i64 as _)));
    }

    if radix == 16 {
        if is_u8 {
            let parsed_value = u8::from_str_radix(value, radix).map_err(|err| {
                make_parse_error(
                    format!("invalid u8 hex literal '{arg_value}': {err}"),
                    span.0,
                    span.1,
                )
            })?;
            Ok(ConstExpr::new(ValueType::Byte(parsed_value as _)))
        } else if !is_long {
            if let Ok(parsed_value) = u32::from_str_radix(value, radix) {
                Ok(ConstExpr::new(ValueType::Int32(parsed_value as i32 as _)))
            } else {
                let parsed_value = u64::from_str_radix(value, radix).map_err(|err| {
                    make_parse_error(
                        format!("invalid hex literal '{arg_value}': {err}"),
                        span.0,
                        span.1,
                    )
                })?;
                Ok(ConstExpr::new(ValueType::Int64(parsed_value as i64 as _)))
            }
        } else {
            let parsed_value = u64::from_str_radix(value, radix).map_err(|err| {
                make_parse_error(
                    format!("invalid hex literal '{arg_value}': {err}"),
                    span.0,
                    span.1,
                )
            })?;
            Ok(ConstExpr::new(ValueType::Int64(parsed_value as i64 as _)))
        }
    } else {
        let parsed_value = i64::from_str_radix(value, radix).map_err(|err| {
            make_parse_error(
                format!("invalid integer literal '{arg_value}': {err}"),
                span.0,
                span.1,
            )
        })?;
        if is_u8 {
            if parsed_value > u8::MAX.into() || parsed_value < 0 {
                return Err(make_parse_error(
                    format!("u8 literal overflow: {parsed_value} is out of range (0..=255)"),
                    span.0,
                    span.1,
                ));
            }
            Ok(ConstExpr::new(ValueType::Byte(parsed_value as i8 as _)))
        } else if is_long {
            Ok(ConstExpr::new(ValueType::Int64(parsed_value as _)))
        } else if parsed_value <= i8::MAX.into() && parsed_value >= i8::MIN.into() {
            Ok(ConstExpr::new(ValueType::Byte(parsed_value as i8 as _)))
        } else if parsed_value <= i32::MAX.into() && parsed_value >= i32::MIN.into() {
            Ok(ConstExpr::new(ValueType::Int32(parsed_value as i32 as _)))
        } else {
            Ok(ConstExpr::new(ValueType::Int64(parsed_value as _)))
        }
    }
}

fn parse_value(pair: pest::iterators::Pair<Rule>) -> Result<ConstExpr, AidlError> {
    match pair.as_rule() {
        Rule::qualified_name => Ok(ConstExpr::new(ValueType::Name(pair.as_str().into()))),
        // A string inside an expression (`A + "y"`); `parse_c_str` validates it.
        Rule::C_STR => parse_c_str(pair),
        Rule::HEXVALUE | Rule::INTVALUE => {
            let span = pair.as_span();
            parse_intvalue(pair.as_str(), (span.start(), span.end()))
        }
        Rule::FLOATVALUE => {
            let span = pair.as_span();
            let value = pair.as_str();
            let value = if let Some(stripped) = value.strip_suffix('f') {
                stripped
            } else {
                value
            };
            let f = value.parse::<f64>().map_err(|_| {
                make_parse_error(
                    format!("invalid float literal: {}", pair.as_str()),
                    span.start(),
                    span.end(),
                )
            })?;
            Ok(ConstExpr::new(ValueType::Double(f as _)))
        }
        Rule::TRUE_LITERAL => Ok(ConstExpr::new(ValueType::Bool(true))),
        Rule::FALSE_LITERAL => Ok(ConstExpr::new(ValueType::Bool(false))),
        _ => unreachable!("Unexpected rule in parse_value(): {}", pair),
    }
}

fn parse_factor(pair: pest::iterators::Pair<Rule>) -> Result<ConstExpr, AidlError> {
    match pair.as_rule() {
        Rule::expression => parse_expression(pair.into_inner()),
        Rule::unary => parse_unary(pair.into_inner()),
        Rule::value => parse_value(pair.into_inner().next().unwrap()),
        _ => unreachable!("Unexpected rule in parse_factor(): {}", pair),
    }
}

fn parse_expression_term(pair: pest::iterators::Pair<Rule>) -> Result<ConstExpr, AidlError> {
    match pair.as_rule() {
        Rule::equality
        | Rule::comparison
        | Rule::bitwise_or
        | Rule::bitwise_xor
        | Rule::bitwise_and
        | Rule::shift
        | Rule::arith
        | Rule::logical_or
        | Rule::logical_and => parse_expression(pair.into_inner()),
        Rule::factor => parse_factor(pair.into_inner().next().unwrap()),
        _ => unreachable!("Unexpected rule in Rule::parse_expression_into: {}", pair),
    }
}

fn parse_expression(mut pairs: pest::iterators::Pairs<Rule>) -> Result<ConstExpr, AidlError> {
    let mut lhs = parse_expression_term(pairs.next().unwrap())?;

    while let Some(pair) = pairs.next() {
        let op = pair.as_str().to_owned();
        let rhs = parse_expression_term(pairs.next().unwrap())?;

        lhs = ConstExpr::new_expr(lhs, &op, rhs)
    }

    Ok(lhs)
}

// Verbatim in Rust `"..."`: non-ASCII passes (AOSP `isValidLiteralChar` bars it); ctrl/`\` fail.
fn parse_c_str(pair: pest::iterators::Pair<Rule>) -> Result<ConstExpr, AidlError> {
    let span = pair.as_span();
    let raw = pair.as_str();
    let inner = &raw[1..raw.len() - 1];
    if let Some(bad) = inner.bytes().find(|&b| b < 0x20 || b == 0x7f || b == b'\\') {
        return Err(make_parse_error(
            format!(
                "invalid byte 0x{bad:02x} in string literal: control characters and \
                 backslash escapes are not allowed (non-ASCII text is permitted)"
            ),
            span.start(),
            span.end(),
        ));
    }
    Ok(ConstExpr::new(ValueType::String(inner.into())))
}

fn parse_const_expr(pair: pest::iterators::Pair<Rule>) -> Result<ConstExpr, AidlError> {
    match pair.as_rule() {
        Rule::constant_value_list => {
            let mut value_list = Vec::new();
            for pair in pair.into_inner() {
                match pair.as_rule() {
                    Rule::const_expr => {
                        // An empty `{}` has no inner pair: diagnose, don't `unwrap()`.
                        let span = pair.as_span();
                        match pair.into_inner().next() {
                            Some(inner) => value_list.push(parse_const_expr(inner)?),
                            None => {
                                return Err(make_parse_error(
                                    "empty `{}` is not a valid constant expression",
                                    span.start(),
                                    span.end(),
                                ))
                            }
                        }
                    }
                    _ => unreachable!("Unexpected rule in Rule::constant_value_list: {}", pair),
                }
            }
            Ok(ConstExpr::new(ValueType::Array(value_list)))
        }

        Rule::CHARVALUE => {
            let span = pair.as_span();
            let (start, end) = (span.start(), span.end());
            // The lexer always matches a quote-delimited `'X'` or `'\X'`.
            let inner = pair
                .as_str()
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
                .ok_or_else(|| make_parse_error("malformed char literal", start, end))?;

            let ch = if let Some(escaped) = inner.strip_prefix('\\') {
                let esc = escaped
                    .chars()
                    .next()
                    .ok_or_else(|| make_parse_error("empty char escape", start, end))?;
                // Wider than AOSP (`'\0'` only); unknown escapes error: `'\a'` is not 'a'.
                match esc {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '0' => '\0',
                    '\\' => '\\',
                    '\'' => '\'',
                    '"' => '"',
                    other => {
                        return Err(make_parse_error(
                            format!("unsupported char escape '\\{other}'"),
                            start,
                            end,
                        ))
                    }
                }
            } else {
                inner
                    .chars()
                    .next()
                    .ok_or_else(|| make_parse_error("empty char literal", start, end))?
            };
            Ok(ConstExpr::new(ValueType::Char(ch)))
        }

        Rule::expression => parse_expression(pair.into_inner()),

        _ => unreachable!("Unexpected rule in parse_const_expr(): {}", pair),
    }
}

fn parse_parameter(pairs: pest::iterators::Pairs<Rule>) -> Result<Parameter, AidlError> {
    let mut parameter = Parameter {
        identifier: "".to_string(),
        const_expr: ConstExpr::default(),
    };

    for pair in pairs {
        match pair.as_rule() {
            Rule::identifier => {
                parameter.identifier = pair.as_str().into();
            }
            Rule::const_expr => {
                let span = pair.as_span();
                match pair.into_inner().next() {
                    Some(inner) => parameter.const_expr = parse_const_expr(inner)?,
                    None => {
                        return Err(make_parse_error(
                            "empty `{}` is not a valid annotation parameter value",
                            span.start(),
                            span.end(),
                        ))
                    }
                }
            }
            _ => unreachable!("Unexpected rule in parse_parameter(): {}", pair),
        }
    }

    Ok(parameter)
}

fn parse_parameter_list(pairs: pest::iterators::Pairs<Rule>) -> Result<Vec<Parameter>, AidlError> {
    let mut list: Vec<Parameter> = Vec::new();
    for pair in pairs {
        let span = pair.as_span();
        let parameter = parse_parameter(pair.into_inner())?;
        // AOSP `aidl_language_y.yy` `parameter_non_empty_list`.
        if list.iter().any(|p| p.identifier == parameter.identifier) {
            return Err(make_parse_error(
                format!("Trying to redefine parameter {}.", parameter.identifier),
                span.start(),
                span.end(),
            ));
        }
        list.push(parameter);
    }

    Ok(list)
}

fn parse_annotation(pairs: pest::iterators::Pairs<Rule>) -> Result<Annotation, AidlError> {
    // The caller sets `annotation_span` from the outer rule so it covers the parens too.
    let mut annotation = Annotation::default();
    for pair in pairs {
        match pair.as_rule() {
            Rule::ANNOTATION => {
                annotation.annotation = pair.as_str().into();
            }

            Rule::const_expr => {
                let span = pair.as_span();
                match pair.into_inner().next() {
                    Some(inner) => {
                        let value = parse_const_expr(inner)?;
                        // AOSP `aidl_language_y.yy`: `@A(expr)` is `@A(value = expr)`.
                        annotation.parameter_list = vec![Parameter {
                            identifier: "value".into(),
                            const_expr: value.clone(),
                        }];
                        annotation.const_expr = Some(value);
                    }
                    None => {
                        return Err(make_parse_error(
                            "empty `{}` is not a valid annotation argument",
                            span.start(),
                            span.end(),
                        ))
                    }
                }
            }

            Rule::parameter_list => {
                annotation.parameter_list = parse_parameter_list(pair.into_inner())?;
            }

            _ => unreachable!("Unexpected rule in parse_annotation(): {}", pair),
        }
    }

    Ok(annotation)
}

/// AOSP `AidlAnnotatable::CheckValid`: of the known annotations only `@JavaPassthrough` repeats.
fn reject_repeated_annotation(
    annotation_list: &[Annotation],
    annotation: &Annotation,
) -> Result<(), AidlError> {
    let name = annotation.annotation.trim_start_matches('@');
    if KNOWN_ANNOTATIONS.contains(&annotation.annotation.as_str())
        && name != "JavaPassthrough"
        && annotation_list
            .iter()
            .any(|a| a.annotation == annotation.annotation)
    {
        return Err(make_invalid_operation_error(
            format!("'{name}' is repeated, but not allowed."),
            annotation.annotation_span,
        ));
    }
    Ok(())
}

fn parse_annotation_list(
    pairs: pest::iterators::Pairs<Rule>,
) -> Result<Vec<Annotation>, AidlError> {
    let mut annotation_list = Vec::new();
    for pair in pairs {
        // The outer `annotation` span covers `@Foo(...)` as well as bare `@Foo`.
        let span = pair.as_span();
        let mut annotation = parse_annotation(pair.into_inner())?;
        annotation.annotation_span = Some((span.start(), span.end()));

        if !KNOWN_ANNOTATIONS.contains(&annotation.annotation.as_str()) {
            let filename = CURRENT_SOURCE_NAME.with(|n| n.borrow().clone());
            CURRENT_WARNINGS.with(|w| {
                w.borrow_mut().push(crate::error::AidlWarning::new(format!(
                    "{}: unknown AIDL annotation '{}' is being ignored",
                    filename, annotation.annotation
                )));
            });
        } else {
            reject_repeated_annotation(&annotation_list, &annotation)?;
            // AOSP `ConstReferenceFinder`; the unfolded form makes it independent of parse order.
            if let Some(reference) = annotation
                .parameter_list
                .iter()
                .find_map(|p| const_reference(&p.const_expr))
            {
                return Err(make_invalid_operation_error(
                    format!(
                        "Value must be a constant expression but contains reference to {reference}."
                    ),
                    annotation.annotation_span,
                ));
            }
        }

        // A misspelt derive errors (as AOSP), not a trait missing later in the user's crate.
        if annotation.annotation == "@RustDerive" {
            if let Some(param) = annotation.parameter_list.iter().find(|p| {
                let name = p.identifier.as_str();
                !RUST_DERIVE_SCHEMA.contains(&name) && !RUST_DERIVE_ALWAYS_EMITTED.contains(&name)
            }) {
                return Err(make_invalid_operation_error(
                    format!(
                        "unknown @RustDerive parameter '{}'; expected one of {}",
                        param.identifier,
                        RUST_DERIVE_SCHEMA.join(", ")
                    ),
                    annotation.annotation_span,
                ));
            }
            // AOSP `CheckValid`: each value must pass `ValueString(boolean)`, i.e. bool or integer.
            if let Some(param) = annotation.parameter_list.iter().find(|p| {
                !matches!(
                    p.const_expr.calculate().map(|c| c.value),
                    Ok(ValueType::Bool(_)
                        | ValueType::Byte(_)
                        | ValueType::Int32(_)
                        | ValueType::Int64(_))
                )
            }) {
                return Err(make_invalid_operation_error(
                    format!(
                        "Invalid value for parameter {} on annotation RustDerive.",
                        param.identifier
                    ),
                    annotation.annotation_span,
                ));
            }
        }

        annotation_list.push(annotation);
    }

    Ok(annotation_list)
}

fn parse_type_args(pairs: pest::iterators::Pairs<Rule>) -> Result<Vec<Type>, AidlError> {
    let mut res = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::r#type => {
                let ty = parse_type(pair.into_inner())?;
                // AOSP `aidl_language_y.yy` refuses only the first argument's; every one here.
                if let Some(annotation) = ty.annotation_list.first() {
                    return Err(make_invalid_operation_error(
                        "Annotations for type arguments are not supported".to_owned(),
                        annotation.annotation_span,
                    ));
                }
                res.push(ty)
            }
            _ => unreachable!("Unexpected rule in parse_type_args(): {}", pair),
        }
    }

    Ok(res)
}

// Shape 1/2 inner: `Generic::type_args` would drop its `<...>` (`X<A<B><C>>`); AOSP rejects it.
fn parse_split_non_array_type(
    pairs: pest::iterators::Pairs<Rule>,
) -> Result<NonArrayType, AidlError> {
    let inner = parse_non_array_type(pairs)?;
    if inner.generic.is_some() {
        return Err(make_invalid_operation_error(
            "Can only specify one set of type parameters.".to_owned(),
            inner.name_span,
        ));
    }
    Ok(inner)
}

fn parse_non_array_type(pairs: pest::iterators::Pairs<Rule>) -> Result<NonArrayType, AidlError> {
    let mut non_array_type = NonArrayType::default();

    for pair in pairs {
        match pair.as_rule() {
            Rule::qualified_name => {
                let span = pair.as_span();
                non_array_type.name = pair.as_str().into();
                non_array_type.name_span = Some((span.start(), span.end()));
            }
            Rule::generic_type1 => {
                let mut pairs = pair.into_inner();
                let generic = Generic::Type1 {
                    type_args1: parse_type_args(pairs.next().unwrap().into_inner())?,
                    non_array_type: parse_split_non_array_type(pairs.next().unwrap().into_inner())?,
                    type_args2: parse_type_args(pairs.next().unwrap().into_inner())?,
                };

                non_array_type.generic = Some(Box::new(generic));
            }

            Rule::generic_type2 => {
                let mut pairs = pair.into_inner();
                let generic = Generic::Type2 {
                    non_array_type: parse_split_non_array_type(pairs.next().unwrap().into_inner())?,
                    type_args: parse_type_args(pairs.next().unwrap().into_inner())?,
                };

                non_array_type.generic = Some(Box::new(generic));
            }
            Rule::generic_type3 => {
                let mut pairs = pair.into_inner();
                let generic = Generic::Type3 {
                    type_args: parse_type_args(pairs.next().unwrap().into_inner())?,
                };

                non_array_type.generic = Some(Box::new(generic));
            }
            _ => {
                unreachable!();
            }
        }
    }

    Ok(non_array_type)
}

fn parse_array_type(pairs: pest::iterators::Pairs<Rule>) -> Result<ArrayType, AidlError> {
    let mut array_type = ArrayType::default();

    for pair in pairs {
        match pair.as_rule() {
            Rule::const_expr => {
                let span = pair.as_span();
                match pair.into_inner().next() {
                    Some(inner) => array_type.const_expr = Some(parse_const_expr(inner)?),
                    None => {
                        return Err(make_parse_error(
                            "empty `{}` is not a valid array dimension",
                            span.start(),
                            span.end(),
                        ))
                    }
                }
            }
            _ => unreachable!("Unexpected rule in parse_array_type(): {}", pair),
        }
    }

    Ok(array_type)
}

fn parse_type(pairs: pest::iterators::Pairs<Rule>) -> Result<Type, AidlError> {
    let mut r#type = Type::default();

    for pair in pairs {
        match pair.as_rule() {
            Rule::annotation_list => {
                r#type.annotation_list = parse_annotation_list(pair.into_inner())?;
            }
            Rule::non_array_type => {
                r#type.non_array_type = parse_non_array_type(pair.into_inner())?;
            }
            Rule::array_type => {
                r#type
                    .array_types
                    .push(parse_array_type(pair.into_inner())?);
            }
            _ => {
                unreachable!("Unexpected rule in parse_type(): {}", pair);
            }
        }
    }

    Ok(r#type)
}

fn parse_variable_decl(
    pairs: pest::iterators::Pairs<Rule>,
    constant: bool,
) -> Result<VariableDecl, AidlError> {
    let mut decl = VariableDecl {
        constant,
        ..Default::default()
    };

    for pair in pairs {
        match pair.as_rule() {
            Rule::annotation_list => {
                decl.annotation_list = parse_annotation_list(pair.into_inner())?;
            }
            Rule::r#type => {
                decl.r#type = parse_type(pair.into_inner())?;
            }
            Rule::identifier => {
                let span = pair.as_span();
                let ident = pair.as_str();
                reject_unrepresentable_identifier(ident, "member name", &span)?;
                decl.identifier = ident.into();
            }
            Rule::const_expr => match pair.into_inner().next() {
                Some(pair) => decl.const_expr = Some(parse_const_expr(pair)?),
                // `= {}` is AOSP's empty array, not "no initializer" (no const `&[T]` default).
                None => decl.const_expr = Some(ConstExpr::new(ValueType::Array(Vec::new()))),
            },
            _ => unreachable!(
                "Unexpected rule in parse_variable_decl(): {}\t{}",
                pair,
                pair.as_str()
            ),
        }
    }

    Ok(decl)
}

fn parse_arg(pairs: pest::iterators::Pairs<Rule>) -> Result<Arg, AidlError> {
    let mut arg = Arg::default();

    for pair in pairs {
        match pair.as_rule() {
            Rule::direction => {
                let span = pair.as_span();
                arg.direction = match pair.as_str() {
                    "in" => Direction::In,
                    "out" => Direction::Out,
                    "inout" => Direction::Inout,
                    _ => {
                        return Err(make_parse_error(
                            format!("unsupported direction: {}", pair.as_str()),
                            span.start(),
                            span.end(),
                        ));
                    }
                };
                arg.direction_span = Some((span.start(), span.end()));
            }
            Rule::r#type => {
                arg.r#type = parse_type(pair.into_inner())?;
            }
            Rule::identifier => {
                // Any name: arguments are emitted as `_arg_<name>`, never raw.
                arg.identifier = pair.as_str().into();
            }
            _ => unreachable!("Unexpected rule in parse_arg(): {}", pair),
        }
    }

    Ok(arg)
}

fn parse_method_decl(pairs: pest::iterators::Pairs<Rule>) -> Result<MethodDecl, AidlError> {
    let mut decl = MethodDecl::default();

    for pair in pairs {
        match pair.as_rule() {
            Rule::annotation_list => {
                decl.annotation_list = parse_annotation_list(pair.into_inner())?;
            }
            Rule::ONEWAY => {
                decl.oneway = true;
            }
            Rule::r#type => {
                decl.r#type = parse_type(pair.into_inner())?;
            }
            Rule::identifier => {
                let span = pair.as_span();
                reject_unrepresentable_identifier(pair.as_str(), "method name", &span)?;
                decl.identifier = pair.as_str().into();
                decl.identifier_span = Some((span.start(), span.end()));
            }
            Rule::arg_list => {
                for pair in pair.into_inner() {
                    match pair.as_rule() {
                        Rule::arg => {
                            decl.arg_list.push(parse_arg(pair.into_inner())?);
                        }
                        _ => unreachable!(
                            "Unexpected rule in parse_method_decl(): {}, \"{}\"",
                            pair,
                            pair.as_str()
                        ),
                    }
                }
            }
            Rule::INTVALUE => {
                let span = pair.as_span();
                let text = pair.as_str();
                // AOSP `ParseInt`: a plain decimal only; a suffix or `_` is refused, not typed.
                let value = text.parse::<i64>().map_err(|_| {
                    make_parse_error(
                        format!("Could not parse int value: {text}"),
                        span.start(),
                        span.end(),
                    )
                })?;
                decl.intvalue = Some(value);
                decl.intvalue_span = Some((span.start(), span.end()));
            }
            _ => unreachable!(
                "Unexpected rule in parse_method_decl(): {}, \"{}\"",
                pair,
                pair.as_str()
            ),
        }
    }

    // AOSP `method_decl`: annotations after `oneway` join the list before it (method level).
    for annotation in std::mem::take(&mut decl.r#type.annotation_list) {
        reject_repeated_annotation(&decl.annotation_list, &annotation)?;
        decl.annotation_list.push(annotation);
    }

    Ok(decl)
}

fn parse_interface_members(
    pairs: pest::iterators::Pairs<Rule>,
    interface: &mut InterfaceDecl,
) -> Result<(), AidlError> {
    for pair in pairs {
        match pair.as_rule() {
            Rule::method_decl => {
                let deprecated = deprecated_at(pair.as_span().start());
                let mut method = parse_method_decl(pair.into_inner())?;
                method.deprecated = deprecated;
                interface.method_list.push(method);
            }

            Rule::constant_decl => {
                let deprecated = deprecated_at(pair.as_span().start());
                let mut constant = parse_variable_decl(pair.into_inner(), true)?;
                constant.deprecated = deprecated;
                interface.constant_list.push(constant);
            }

            Rule::decl => {
                let deprecated = deprecated_at(pair.as_span().start());
                let mut members = parse_decl(pair.into_inner())?;
                for member in &mut members {
                    member.set_deprecated(deprecated.clone());
                }
                interface.members.append(&mut members);
            }

            _ => unreachable!("Unexpected rule in parse_interface_members(): {}", pair),
        }
    }
    Ok(())
}

fn parse_interface_decl(
    annotation_list: Vec<Annotation>,
    pairs: pest::iterators::Pairs<Rule>,
) -> Result<Declaration, AidlError> {
    let mut interface = InterfaceDecl {
        annotation_list,
        ..Default::default()
    };

    for pair in pairs {
        match pair.as_rule() {
            Rule::ONEWAY => {
                interface.oneway = true;
            }

            Rule::qualified_name => {
                let span = pair.as_span();
                reject_unrepresentable_identifier(pair.as_str(), "interface name", &span)?;
                reject_qualified_type_name(pair.as_str(), &span)?;
                interface.name = pair.as_str().into();
                interface.name_span = Some((span.start(), span.end()));
            }

            Rule::interface_members => {
                parse_interface_members(pair.into_inner(), &mut interface)?;
            }

            _ => unreachable!("Unexpected rule in parse_interface_decl(): {}", pair),
        }
    }

    validate_oneway_methods(&interface)?;

    Ok(Declaration::Interface(interface))
}

fn parse_parcelable_members(
    pairs: pest::iterators::Pairs<Rule>,
) -> Result<Vec<Declaration>, AidlError> {
    let mut res = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::variable_decl | Rule::constant_decl => {
                let constant = pair.as_rule() == Rule::constant_decl;
                let deprecated = deprecated_at(pair.as_span().start());
                let mut var = parse_variable_decl(pair.into_inner(), constant)?;
                var.deprecated = deprecated;
                res.push(Declaration::Variable(var));
            }
            Rule::decl => {
                let deprecated = deprecated_at(pair.as_span().start());
                let mut members = parse_decl(pair.into_inner())?;
                for member in &mut members {
                    member.set_deprecated(deprecated.clone());
                }
                res.append(&mut members);
            }
            _ => unreachable!("Unexpected rule in parse_parcelable_members(): {}", pair),
        }
    }

    Ok(res)
}

/// A declaration's type parameter; its annotation is a requirement on the use-site argument.
#[derive(Debug, Default, Clone)]
pub struct TypeParam {
    pub name: String,
    pub name_span: Option<(usize, usize)>,
    pub annotation_list: Vec<Annotation>,
}

/// Annotations AOSP accepts on a type parameter (`AllSchemas` entries with `CONTEXT_TYPE_PARAM`).
const TYPE_PARAM_ANNOTATIONS: &[&str] = &[
    "@FixedSize",
    "@VintfStability",
    "@JavaPassthrough",
    "@JavaSuppressLint",
];

fn parse_type_param(pairs: pest::iterators::Pairs<Rule>) -> Result<TypeParam, AidlError> {
    let mut param = TypeParam::default();
    for pair in pairs {
        match pair.as_rule() {
            Rule::annotation_list => {
                param
                    .annotation_list
                    .append(&mut parse_annotation_list(pair.into_inner())?);
            }
            Rule::identifier => {
                let span = pair.as_span();
                reject_unrepresentable_identifier(pair.as_str(), "type parameter", &span)?;
                param.name = pair.as_str().into();
                param.name_span = Some((span.start(), span.end()));
            }
            _ => unreachable!("Unexpected rule in parse_type_param(): {}", pair),
        }
    }
    if let Some(other) = param
        .annotation_list
        .iter()
        .find(|a| !TYPE_PARAM_ANNOTATIONS.contains(&a.annotation.as_str()))
    {
        return Err(make_invalid_operation_error(
            format!(
                "'{}' cannot annotate the type parameter '{}': only @FixedSize and \
                 @VintfStability state a requirement on a type argument (AOSP also \
                 admits the Java-only @JavaPassthrough and @JavaSuppressLint)",
                other.annotation, param.name
            ),
            other.annotation_span,
        ));
    }
    Ok(param)
}

fn parse_optional_type_params(
    pairs: pest::iterators::Pairs<Rule>,
) -> Result<Vec<TypeParam>, AidlError> {
    let mut res: Vec<TypeParam> = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::type_param => {
                let param = parse_type_param(pair.into_inner())?;
                // AOSP `AidlParameterizable::CheckValid`: "Type parameter 'T' is repeated."
                if res.iter().any(|p| p.name == param.name) {
                    return Err(make_invalid_operation_error(
                        format!("type parameter '{}' is repeated", param.name),
                        param.name_span,
                    ));
                }
                res.push(param);
            }
            _ => unreachable!("Unexpected rule in parse_optional_type_params(): {}", pair),
        }
    }

    Ok(res)
}

fn parse_unstructured_parcelable(
    parcelable: &mut ParcelableDecl,
    mut pairs: pest::iterators::Pairs<Rule>,
) -> Result<(), AidlError> {
    enum HeaderType {
        CppHeader,
        NdkHeader,
        RustType,
    }

    let (first, second) = pairs
        .next()
        .zip(pairs.next())
        .expect("Incomplete rule in parse_unstructured_parcelable()");

    let header = match first.as_rule() {
        Rule::CPP_HEADER => HeaderType::CppHeader,
        Rule::NDK_HEADER => HeaderType::NdkHeader,
        Rule::RUST_TYPE => HeaderType::RustType,
        _ => unreachable!(
            "Unexpected rule in parse_unstructured_parcelable(): {}",
            first
        ),
    };

    match second.as_rule() {
        Rule::C_STR => {
            let str = second.as_str();
            let str = str[1..str.len() - 1].into();
            match header {
                HeaderType::CppHeader => parcelable.cpp_header = str,
                HeaderType::NdkHeader => parcelable.ndk_header = str,
                HeaderType::RustType => parcelable.rust_type = str,
            }
        }
        _ => unreachable!(
            "Unexpected rule in parse_unstructured_parcelable(): {}",
            second
        ),
    }

    Ok(())
}

fn parse_parcelable_decl(
    annotation_list: Vec<Annotation>,
    is_unstructured: bool,
    pairs: pest::iterators::Pairs<Rule>,
) -> Result<Declaration, AidlError> {
    let mut parcelable = ParcelableDecl {
        annotation_list,
        is_unstructured,
        ..Default::default()
    };

    for pair in pairs {
        match pair.as_rule() {
            Rule::qualified_name => {
                let span = pair.as_span();
                reject_unrepresentable_identifier(pair.as_str(), "parcelable name", &span)?;
                if !is_unstructured {
                    reject_qualified_type_name(pair.as_str(), &span)?;
                }
                parcelable.name_span = Some((span.start(), span.end()));
                parcelable.name = pair.as_str().into();
            }

            Rule::optional_type_params => {
                parcelable.type_params = parse_optional_type_params(pair.into_inner())?;
            }

            Rule::parcelable_members => {
                parcelable
                    .members
                    .append(&mut parse_parcelable_members(pair.into_inner())?);
            }

            Rule::optional_unstructured_headers => {
                parse_unstructured_parcelable(&mut parcelable, pair.into_inner())?;
            }

            _ => unreachable!("Unexpected rule in parse_parcelable_decl(): {}", pair),
        }
    }

    // `rust_type` emits `pub mod <name>`, so a dotted name would be a Rust syntax error.
    if !parcelable.rust_type.is_empty() && parcelable.name.contains('.') {
        let (start, end) = parcelable.name_span.unwrap_or((0, 0));
        return Err(make_parse_error(
            format!(
                "parcelable '{}' with rust_type can't have a qualified name; use `package`",
                parcelable.name
            ),
            start,
            end,
        ));
    }

    Ok(Declaration::Parcelable(parcelable))
}

#[derive(Debug, Default, Clone)]
pub struct Enumerator {
    pub identifier: String,
    pub const_expr: Option<ConstExpr>,
    pub deprecated: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct EnumDecl {
    pub namespace: Namespace,
    pub annotation_list: Vec<Annotation>,
    pub name: String,
    pub name_span: Option<(usize, usize)>,
    pub enumerator_list: Vec<Enumerator>,
    pub members: Vec<Declaration>,
    /// Synthetic marker: AIDL gives every `union Foo { ... }` an implicit
    /// nested `Tag` enum (one variant per field) that downstream types
    /// may reference as `Foo.Tag`. `calculate_namespace` injects such a
    /// stub `EnumDecl` into `DECLARATION_MAP` so `Foo.Tag` resolves like
    /// any other user-defined type; `tag_of_union` then stores the
    /// parent union's full namespace so `lookup_decl_from_name` can
    /// hand that namespace back as the codegen-effective module path
    /// (the `Tag` struct lives *inside* `mod Foo`, sibling to the union
    /// enum, so the standard `<ns>::<name>` doubling would emit
    /// `Foo::Tag::Tag` — using the union ns yields `Foo::Tag`). The
    /// runtime `Tag` struct itself is emitted by the union template in
    /// [`crate::generator`], not from this stub.
    pub tag_of_union: Option<Namespace>,
    pub deprecated: Option<String>,
}

fn parse_enumerator(pairs: pest::iterators::Pairs<Rule>) -> Result<Enumerator, AidlError> {
    let mut res = Enumerator::default();

    for pair in pairs {
        match pair.as_rule() {
            Rule::identifier => {
                let span = pair.as_span();
                reject_unrepresentable_identifier(pair.as_str(), "enum member name", &span)?;
                res.identifier = pair.as_str().into();
            }
            Rule::const_expr => {
                let span = pair.as_span();
                match pair.into_inner().next() {
                    Some(inner) => res.const_expr = Some(parse_const_expr(inner)?),
                    None => {
                        return Err(make_parse_error(
                            "empty `{}` is not a valid enumerator value",
                            span.start(),
                            span.end(),
                        ))
                    }
                }
            }
            _ => unreachable!("Unexpected rule in parse_enumerator(): {}", pair),
        }
    }

    Ok(res)
}

fn parse_enum_decl(
    annotation_list: Vec<Annotation>,
    pairs: pest::iterators::Pairs<Rule>,
) -> Result<Declaration, AidlError> {
    let mut enum_decl = EnumDecl {
        annotation_list: annotation_list.clone(),
        ..Default::default()
    };

    for pair in pairs {
        match pair.as_rule() {
            Rule::qualified_name => {
                let span = pair.as_span();
                reject_unrepresentable_identifier(pair.as_str(), "enum name", &span)?;
                reject_qualified_type_name(pair.as_str(), &span)?;
                enum_decl.name = pair.as_str().into();
                enum_decl.name_span = Some((span.start(), span.end()));
            }
            Rule::enumerator => {
                let deprecated = deprecated_at(pair.as_span().start());
                let mut enumerator = parse_enumerator(pair.into_inner())?;
                enumerator.deprecated = deprecated;
                enum_decl.enumerator_list.push(enumerator);
            }
            _ => unreachable!("Unexpected rule in parse_enum_decl(): {}", pair),
        }
    }

    Ok(Declaration::Enum(enum_decl))
}

#[derive(Debug, Default, Clone)]
pub struct UnionDecl {
    pub namespace: Namespace,
    pub annotation_list: Vec<Annotation>,
    pub name: String,
    pub name_span: Option<(usize, usize)>,
    pub type_params: Vec<TypeParam>,
    pub members: Vec<Declaration>,
    pub deprecated: Option<String>,
}

fn parse_union_decl(
    annotation_list: Vec<Annotation>,
    pairs: pest::iterators::Pairs<Rule>,
) -> Result<Declaration, AidlError> {
    let mut union_decl = UnionDecl {
        annotation_list,
        ..Default::default()
    };

    for pair in pairs {
        match pair.as_rule() {
            Rule::qualified_name => {
                let span = pair.as_span();
                reject_unrepresentable_identifier(pair.as_str(), "union name", &span)?;
                reject_qualified_type_name(pair.as_str(), &span)?;
                union_decl.name = pair.as_str().into();
                union_decl.name_span = Some((span.start(), span.end()));
            }
            Rule::optional_type_params => {
                union_decl.type_params = parse_optional_type_params(pair.into_inner())?;
            }
            Rule::parcelable_members => {
                union_decl.members = parse_parcelable_members(pair.into_inner())?;
            }
            _ => unreachable!("Unexpected rule in parse_union_decl(): {}", pair),
        }
    }
    Ok(Declaration::Union(union_decl))
}

fn parse_decl(pairs: pest::iterators::Pairs<Rule>) -> Result<Vec<Declaration>, AidlError> {
    let mut annotation_list = Vec::new();
    let mut declarations = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::annotation_list => {
                annotation_list = parse_annotation_list(pair.into_inner())?;
            }
            Rule::interface_decl => {
                declarations.push(parse_interface_decl(
                    annotation_list.clone(),
                    pair.into_inner(),
                )?);
            }

            Rule::parcelable_decl => {
                declarations.push(parse_parcelable_decl(
                    annotation_list.clone(),
                    pair.as_str().ends_with(';'),
                    pair.into_inner(),
                )?);
            }
            Rule::enum_decl => {
                declarations.push(parse_enum_decl(annotation_list.clone(), pair.into_inner())?);
            }
            Rule::union_decl => {
                declarations.push(parse_union_decl(
                    annotation_list.clone(),
                    pair.into_inner(),
                )?);
            }

            _ => unreachable!("Unexpected rule in parse_decl(): {}", pair),
        };
    }

    Ok(declarations)
}

fn calculate_namespace(
    decl: &mut Declaration,
    mut namespace: Namespace,
    document_context: &DocumentContext,
) {
    if decl.is_variable().is_some() {
        return;
    }

    namespace.push(decl.name());

    decl.set_namespace(namespace.clone());

    DECLARATION_MAP.with(|hashmap| {
        hashmap.borrow_mut().insert(namespace.clone(), decl.clone());
    });
    DECLARATION_DOCUMENT_MAP.with(|hashmap| {
        hashmap
            .borrow_mut()
            .insert(namespace.clone(), document_context.clone());
    });

    // Stub `Tag` enum only so `<Union>.Tag` resolves; codegen: see `EnumDecl::tag_of_union`.
    if matches!(decl, Declaration::Union(_)) {
        let mut tag_ns = namespace.clone();
        tag_ns.push("Tag");
        let tag_enum = Declaration::Enum(EnumDecl {
            namespace: tag_ns.clone(),
            name: "Tag".into(),
            tag_of_union: Some(namespace.clone()),
            ..Default::default()
        });
        DECLARATION_MAP.with(|hashmap| {
            hashmap.borrow_mut().insert(tag_ns.clone(), tag_enum);
        });
        DECLARATION_DOCUMENT_MAP.with(|hashmap| {
            hashmap
                .borrow_mut()
                .insert(tag_ns, document_context.clone());
        });
    }

    for decl in decl.members_mut() {
        calculate_namespace(decl, namespace.clone(), document_context);
    }
}

/// Max `()[]{}` depth: far above real AIDL, far below the recursive parser's stack overflow.
const MAX_NESTING_DEPTH: usize = 256;

// Generic re-parse is exponential (~3.7x/level), not just deep; AOSP's deepest vendored is 3.
const MAX_GENERIC_DEPTH: usize = 12;

/// Operators per statement/element: a bracket-free `1+1+...` chain recurses once per operator.
const MAX_OPERATOR_RUN: usize = 1024;

#[derive(Debug, Clone, Copy)]
pub enum NestingLimit {
    Bracket,
    Generic,
    OperatorRun,
}

impl NestingLimit {
    pub fn describe(self) -> &'static str {
        match self {
            NestingLimit::Bracket => "brackets are nested too deeply",
            NestingLimit::Generic => "generic types are nested too deeply",
            NestingLimit::OperatorRun => "too many operators in one expression",
        }
    }
}

// Pre-parse: deep nesting/operator chains overflow the stack (SIGABRT) before `MAX_EXPR_DEPTH`.
fn check_nesting_depth(source: &str) -> Option<(usize, NestingLimit, usize)> {
    let bytes = source.as_bytes();
    let mut i = 0;
    let mut bracket_depth: usize = 0; // () [] {}
                                      // Open `<`s; only a `>`-closed one is generic (`0 < 1`).
    let mut angle_open: Vec<usize> = Vec::new();
    let mut generic_depth: usize = 0;
    let mut op_run: usize = 0; // operator tokens in the current statement/element
    let next = |i: usize| bytes.get(i + 1).copied();
    while i < bytes.len() {
        match bytes[i] {
            // Skip string/char literals (with escapes): brackets in text don't count.
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        c if c == q => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                continue;
            }
            b'/' if next(i) == Some(b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if next(i) == Some(b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            // A bracket/element/statement boundary restarts the operator run.
            b'(' | b'[' | b'{' => {
                bracket_depth += 1;
                op_run = 0;
            }
            b')' | b']' | b'}' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                op_run = 0;
            }
            b',' => op_run = 0,
            // `<<` shift / `<=` are not generic openers.
            b'<' if matches!(next(i), Some(b'<') | Some(b'=')) => {
                if bytes[i + 1] == b'<' {
                    op_run += 1; // `<<` drives one shift-recursion level
                }
                i += 2;
                continue;
            }
            b'<' => {
                // A type argument starts with a name, an annotation or a comment; a digit cannot.
                let next_tok = bytes[i + 1..].iter().find(|b| !b.is_ascii_whitespace());
                if next_tok.is_some_and(|b| b.is_ascii_alphabetic() || b"_@/".contains(b)) {
                    angle_open.push(i);
                }
            }
            // `>>` closes two open generics (`Map<int, List<int>>`), else it is a shift.
            b'>' if next(i) == Some(b'>') => {
                if angle_open.len() >= 2 {
                    generic_depth = generic_depth.max(angle_open.len());
                    angle_open.truncate(angle_open.len() - 2);
                } else {
                    angle_open.clear();
                    op_run += 1;
                }
                i += 2;
                continue;
            }
            // `>=` is a comparison, never a generic closer.
            b'>' if next(i) == Some(b'=') => {
                i += 2;
                continue;
            }
            b'>' if !angle_open.is_empty() => {
                generic_depth = generic_depth.max(angle_open.len());
                angle_open.pop();
            }
            b';' => {
                angle_open.clear();
                op_run = 0;
            }
            // Each operator is one recursion level; `op_run` bounds unbracketed chains.
            b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^' | b'!' | b'~' => op_run += 1,
            _ => {}
        }
        if bracket_depth > MAX_NESTING_DEPTH {
            return Some((i, NestingLimit::Bracket, MAX_NESTING_DEPTH));
        }
        if generic_depth > MAX_GENERIC_DEPTH {
            return Some((i, NestingLimit::Generic, MAX_GENERIC_DEPTH));
        }
        // An unclosed `<` costs the same exponential re-parse as a closed one.
        if angle_open.len() > MAX_GENERIC_DEPTH {
            return Some((i, NestingLimit::Generic, MAX_GENERIC_DEPTH));
        }
        if op_run > MAX_OPERATOR_RUN {
            return Some((i, NestingLimit::OperatorRun, MAX_OPERATOR_RUN));
        }
        i += 1;
    }
    None
}

pub fn parse_document(ctx: &SourceContext) -> Result<Document, AidlError> {
    let _guard = SourceGuard::new(&ctx.filename, &ctx.source);
    // Reject pathological nesting before the recursive pest parser overflows the stack.
    if let Some((offset, limit, max)) = check_nesting_depth(&ctx.source) {
        return Err(ParseError::nesting_too_deep(
            &ctx.filename,
            &ctx.source,
            offset,
            limit.describe(),
            max,
        )
        .into());
    }
    reset_enum_resolution_state();
    // Drop leftovers from a previous call so the warnings are scoped to this parse.
    CURRENT_WARNINGS.with(|w| w.borrow_mut().clear());
    let mut document = Document::new();

    match AIDLParser::parse(Rule::document, &ctx.source) {
        Ok(pairs) => {
            for pair in pairs {
                match pair.as_rule() {
                    Rule::package => {
                        let name = pair.into_inner().next().unwrap();
                        reject_unrepresentable_identifier(
                            name.as_str(),
                            "package segment",
                            &name.as_span(),
                        )?;
                        document.package = Some(name.as_str().into());
                    }

                    Rule::imports => {
                        for pair in pair.into_inner() {
                            let import = pair.as_str().to_string();
                            let key = match import.rfind('.') {
                                Some(idx) => &import[(idx + 1)..],
                                None => &import,
                            };
                            // Simple-name clash: AOSP errors, we warn (same FQN twice is fine).
                            if let Some(existing) = document.imports.get(key) {
                                if existing != &import {
                                    CURRENT_WARNINGS.with(|w| {
                                        w.borrow_mut().push(crate::error::AidlWarning::new(
                                            format!(
                                                "duplicate import of simple name '{key}': \
                                                 '{existing}' is shadowed by '{import}'; an \
                                                 unqualified reference to '{key}' resolves to \
                                                 the latter"
                                            ),
                                        ));
                                    });
                                }
                            }
                            document.imports.insert(key.into(), import);
                        }
                    }

                    Rule::decl => {
                        let deprecated = deprecated_at(pair.as_span().start());
                        let mut decls = parse_decl(pair.into_inner())?;
                        for decl in &mut decls {
                            decl.set_deprecated(deprecated.clone());
                        }
                        document.decls.append(&mut decls);
                    }

                    Rule::EOI => {}

                    _ => {
                        unreachable!("Unexpected rule in parse_document(): {}", pair)
                    }
                }
            }
        }
        Err(err) => {
            return Err(pest_error_to_diagnostic(err, &ctx.filename, &ctx.source).into());
        }
    }

    let namespace = if let Some(ref package) = document.package {
        Namespace::new(package, Namespace::AIDL)
    } else {
        Namespace::default()
    };

    let document_context = DocumentContext::from_document(&document);
    for decl in &mut document.decls {
        calculate_namespace(decl, namespace.clone(), &document_context);
    }

    // Move this parse's warnings into the document so they don't leak into the next.
    document.warnings = CURRENT_WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut()));

    Ok(document)
}

pub fn reset() {
    DECLARATION_MAP.with(|hashmap| {
        hashmap.borrow_mut().clear();
    });
    DECLARATION_DOCUMENT_MAP.with(|hashmap| {
        hashmap.borrow_mut().clear();
    });
    NAMESPACE_STACK.with(|stack| {
        stack.borrow_mut().clear();
    });
    DOCUMENT.with(|doc| {
        *doc.borrow_mut() = Document::new();
    });
    SYMBOL_TABLE.with(|table| {
        table.borrow_mut().clear();
    });
    CURRENT_WARNINGS.with(|w| {
        w.borrow_mut().clear();
    });
    BUILTIN_RUST_PATHS.with(|map| {
        map.borrow_mut().clear();
    });
    reset_enum_resolution_state();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn test_second_type_parameter_set_is_rejected() -> Result<(), Box<dyn Error>> {
        // Shape 2 with inner `A<B>`: `Generic::type_args` would drop `<B>` and yield `List<A<C>>`.
        let ctx = SourceContext::new("p.aidl", "parcelable P { List<A<B><C>> x; }");
        let err = parse_document(&ctx).expect_err("second `<...>` must be rejected");
        assert!(
            err.to_string()
                .contains("Can only specify one set of type parameters"),
            "{err}"
        );

        let ctx = SourceContext::new("p.aidl", "parcelable P { List<A<B>> x; }");
        parse_document(&ctx)?;
        Ok(())
    }

    #[test]
    fn test_parse_string_concat_expression() -> Result<(), Box<dyn Error>> {
        // String literals participate in the ordinary expression grammar.
        let mut res =
            AIDLParser::parse(Rule::expression, r##""Hello" + " World""##).map_err(|err| {
                println!("{err}");
                err
            })?;

        let expr = parse_expression(res.next().unwrap().into_inner())?;
        assert_eq!(
            expr.calculate()?,
            ConstExpr::new(ValueType::String("Hello World".into()))
        );

        Ok(())
    }

    #[test]
    fn test_parse_expression() -> Result<(), Box<dyn Error>> {
        let mut res =
            AIDLParser::parse(Rule::expression, r##"1 + 3 * 2 << 2 | 4"##).map_err(|err| {
                println!("{err}");
                err
            })?;

        let expr = parse_expression(res.next().unwrap().into_inner())?;

        // ((1 + 3*2) << 2) | 4 = 28 | 4 = 28
        assert_eq!(
            expr.calculate().unwrap(),
            ConstExpr::new(ValueType::Int64(28))
        );

        // A negative left-shift operand is an AOSP overflow diagnostic (OverflowGuard).
        let mut res = AIDLParser::parse(Rule::expression, r##"1 + -3 * 2 << 2"##)?;
        let expr = parse_expression(res.next().unwrap().into_inner())?;
        assert!(expr.calculate().is_err());

        Ok(())
    }

    #[test]
    fn test_bitwise_precedence_matches_aosp() -> Result<(), Box<dyn Error>> {
        // AOSP aidl_language_y.yy: bitwise |/^/& bind looser than ==/!= and relational ops.
        for (src, expected) in [
            ("1 & 2 == 2", 1),       // 1 & (2 == 2), not (1 & 2) == 2
            ("4 | 2 != 2", 4),       // 4 | (2 != 2), not (4 | 2) != 2
            ("(1 & 2) == 2", 0),     // explicit parens group first
            ("1 | 2 ^ 3 & 2", 1),    // | < ^ < & among themselves
            ("1 << 2 < 8", 1),       // shift still binds tighter than comparison
            ("1 == 1 && 2 == 2", 1), // && stays looser than bitwise/equality
        ] {
            let mut res = AIDLParser::parse(Rule::expression, src)?;
            let calc = parse_expression(res.next().unwrap().into_inner())?.calculate()?;
            assert_eq!(calc.value.to_i64()?, expected, "{src}");
        }
        Ok(())
    }

    #[test]
    fn test_flat_interface_with_thousands_of_members_parses() -> Result<(), Box<dyn Error>> {
        // Per-member recursion would overflow here; `check_nesting_depth` does not count members.
        let mut src = String::from("package test.pkg;\ninterface IBig {\n    const int K = 1;\n");
        for i in 0..5000 {
            src.push_str(&format!("    void method{i}();\n"));
        }
        src.push_str("}\n");

        let ctx = SourceContext::new("big.aidl", src);
        let doc = parse_document(&ctx)?;
        match &doc.decls[0] {
            Declaration::Interface(interface) => {
                assert_eq!(interface.method_list.len(), 5000);
                assert_eq!(interface.constant_list.len(), 1);
            }
            decl => panic!("expected interface, got {decl:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_nesting_depth_guard_rejects_deep_input() {
        // The pre-scan must flag deep parens/generics before they reach the recursive parser.
        let deep_parens = format!("{}1{}", "(".repeat(1000), ")".repeat(1000));
        assert!(check_nesting_depth(&deep_parens).is_some());
        let deep_generics = format!("{}int{}", "List<".repeat(1000), ">".repeat(1000));
        assert!(check_nesting_depth(&deep_generics).is_some());
        // A normal document (and shift/comparison operators) must not trip.
        assert!(check_nesting_depth("interface IFoo { void m(); }").is_none());
        assert!(check_nesting_depth("const int X = 1 << 8 >> 2; const int Y = 3;").is_none());
    }

    #[test]
    fn test_nesting_depth_guard_rejects_unclosed_generics() {
        let unclosed = format!(
            "parcelable P {{ {}int x; }}",
            "List<".repeat(MAX_GENERIC_DEPTH + 1)
        );
        assert!(matches!(
            check_nesting_depth(&unclosed),
            Some((_, NestingLimit::Generic, MAX_GENERIC_DEPTH))
        ));
        let at_limit = format!(
            "parcelable P {{ {}int x; }}",
            "List<".repeat(MAX_GENERIC_DEPTH)
        );
        assert!(check_nesting_depth(&at_limit).is_none());
    }

    #[test]
    fn test_intvalue_underscores_and_unsigned_suffixes() -> Result<(), Box<dyn Error>> {
        // AOSP digit separators and u32/u64 suffixes.
        assert_eq!(
            parse_intvalue("1_000_000", (0, 0))?.value,
            ValueType::Int32(1_000_000)
        );
        assert_eq!(
            parse_intvalue("0xFF_FF", (0, 0))?.value,
            ValueType::Int32(0xFFFF)
        );
        assert_eq!(parse_intvalue("10u32", (0, 0))?.value, ValueType::Int32(10));
        assert_eq!(parse_intvalue("10u64", (0, 0))?.value, ValueType::Int64(10));
        Ok(())
    }

    #[test]
    fn test_floatvalue_without_decimal_point() {
        // Each float shape is spelled out, since the PEG cannot backtrack leading digits.
        for s in ["5f", "10f", "1e10", "1E5", "3.14", ".5"] {
            assert!(
                AIDLParser::parse(Rule::FLOATVALUE, s).is_ok(),
                "FLOATVALUE should accept {s}"
            );
        }
        // A bare integer is NOT a float (must fall through to INTVALUE).
        assert!(AIDLParser::parse(Rule::FLOATVALUE, "5").is_err());
    }

    #[test]
    fn test_logical_not_is_not_bitwise() -> Result<(), Box<dyn Error>> {
        // `!5` is logical negation (false), not bitwise complement (-6); `!0` is true.
        let mut res = AIDLParser::parse(Rule::expression, "!5")?;
        let calc = parse_expression(res.next().unwrap().into_inner())?.calculate()?;
        assert_eq!(calc.value, ValueType::Bool(false));

        let mut res0 = AIDLParser::parse(Rule::expression, "!0")?;
        let calc0 = parse_expression(res0.next().unwrap().into_inner())?.calculate()?;
        assert_eq!(calc0.value, ValueType::Bool(true));
        Ok(())
    }

    #[test]
    fn test_namespace_guard() {
        let _ns_1 = NamespaceGuard::new(&Namespace::new("1.1", Namespace::AIDL));
        {
            assert_eq!(current_namespace(), Namespace::new("1.1", Namespace::AIDL));
            let _ns_2 = NamespaceGuard::new(&Namespace::new("2.2", Namespace::AIDL));
            {
                assert_eq!(current_namespace(), Namespace::new("2.2", Namespace::AIDL));
                let _ns_3 = NamespaceGuard::new(&Namespace::new("3.3", Namespace::AIDL));
                assert_eq!(current_namespace(), Namespace::new("3.3", Namespace::AIDL));
            }
            assert_eq!(current_namespace(), Namespace::new("2.2", Namespace::AIDL));
        }
    }

    // thread-local state is cleared after SourceGuard is dropped
    #[test]
    fn test_source_guard_cleanup_on_drop() {
        {
            let _guard = SourceGuard::new("test.aidl", "source text");
            assert_eq!(current_source_name(), "test.aidl");
            assert_eq!(current_source_text(), "source text");
        }
        // After drop, thread-locals should be cleared
        assert_eq!(current_source_name(), "");
        assert_eq!(current_source_text(), "");
    }

    // thread-local state is cleared even when a panic occurs inside SourceGuard
    #[test]
    fn test_source_guard_cleanup_on_panic() {
        let result = std::panic::catch_unwind(|| {
            let _guard = SourceGuard::new("panic.aidl", "panic source");
            panic!("intentional panic to test cleanup");
        });
        assert!(result.is_err());
        assert_eq!(current_source_name(), "");
        assert_eq!(current_source_text(), "");
    }
}
