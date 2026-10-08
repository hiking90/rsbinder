// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use crate::error::{pest_error_to_diagnostic, AidlError, ConstExprError, ParseError};

use pest::Parser;
#[derive(pest_derive::Parser)]
#[grammar = "aidl.pest"]
pub struct AIDLParser;

use crate::const_expr::{ConstExpr, RefKind, ValueType};
use crate::type_generator;
use crate::Namespace;

thread_local! {
    static DECLARATION_MAP: RefCell<HashMap<Namespace, Declaration>> = RefCell::new(HashMap::new());
    static DECLARATION_DOCUMENT_MAP: RefCell<HashMap<Namespace, DocumentContext>> = RefCell::new(HashMap::new());
    static NAMESPACE_STACK: RefCell<Vec<Namespace>> = const { RefCell::new(Vec::new()) };
    static DOCUMENT: RefCell<DocumentContext> = RefCell::new(DocumentContext::default());

    // Final value of each constant and enum member by `Symbol::key`; `None` while it is resolving.
    static CONST_VALUES: RefCell<HashMap<String, Option<Result<ConstExpr, String>>>> = RefCell::new(HashMap::new());
    // Live `resolve_symbol` calls; each is iterative, so only re-entry would add stack.
    static RESOLVE_NESTING: Cell<usize> = const { Cell::new(0) };

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
    /// AOSP reads javadoc tags from block comments only.
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

/// First `@deprecated` block in the comment run before `start` (AOSP FindDeprecated, 14.0.0_r50+).
pub fn deprecated_at(start: usize) -> Option<String> {
    CURRENT_COMMENTS.with(|spans| {
        let spans = spans.borrow();
        CURRENT_SOURCE_TEXT.with(|text| {
            let text = text.borrow();
            let end = spans.partition_point(|c| c.end <= start);
            // Walk back while only whitespace separates a comment from what follows it.
            let mut first = end;
            let mut next_start = start;
            while let Some(span) = first.checked_sub(1).map(|i| spans[i]) {
                let gap = text.get(span.end..next_start)?;
                if !gap.chars().all(char::is_whitespace) {
                    break;
                }
                first -= 1;
                next_start = span.start;
            }
            spans[first..end]
                .iter()
                .filter(|span| span.is_block)
                .find_map(|span| find_deprecated(text.get(span.start..span.end)?))
        })
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

fn reset_const_values() {
    CONST_VALUES.with(|values| values.borrow_mut().clear());
}

pub fn set_current_document(document: &Document) {
    let context = DocumentContext::from_document(document);
    set_current_document_context(&context);
}

fn set_current_document_context(context: &DocumentContext) {
    DOCUMENT.with(|doc| *doc.borrow_mut() = context.clone())
}

fn current_document_context() -> DocumentContext {
    DOCUMENT.with(|doc| doc.borrow().clone())
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

#[derive(Debug)]
pub struct LookupDecl {
    pub decl: Declaration,
    pub ns: Namespace,
    pub name: Namespace,
}

pub fn lookup_decl_from_name(name: &str, style: &str) -> Option<LookupDecl> {
    let found = locate_decl(name, style)?;
    let decl = with_decl(&found.key, Declaration::clone)?;
    Some(LookupDecl {
        decl,
        ns: found.ns,
        name: found.name,
    })
}

/// [`lookup_decl_from_name`] without cloning the declaration; `key` is its `DECLARATION_MAP` entry.
struct DeclLocation {
    key: Namespace,
    ns: Namespace,
    name: Namespace,
}

fn with_decl<R>(key: &Namespace, f: impl FnOnce(&Declaration) -> R) -> Option<R> {
    DECLARATION_MAP.with(|map| map.borrow().get(key).map(f))
}

fn locate_decl(name: &str, style: &str) -> Option<DeclLocation> {
    let namespace = Namespace::new(name, style);
    let key = bind_type_name(&namespace)?;
    decl_location(key, namespace)
}

/// [`lookup_decl_from_name`] for a name already resolved: it is the key, never bound again.
pub(crate) fn lookup_decl_from_canonical(name: &str) -> Option<LookupDecl> {
    let namespace = Namespace::new(name, Namespace::AIDL);
    let found = decl_location(namespace.clone(), namespace)?;
    let decl = with_decl(&found.key, Declaration::clone)?;
    Some(LookupDecl {
        decl,
        ns: found.ns,
        name: found.name,
    })
}

fn decl_location(key: Namespace, mut namespace: Namespace) -> Option<DeclLocation> {
    // A union `Tag` sits in `mod <Union>`: use the union's ns (`<Union>::Tag`, not `Tag::Tag`).
    let tag_of_union = with_decl(&key, |decl| match decl {
        Declaration::Enum(e) => e.tag_of_union.clone(),
        _ => None,
    })?;

    // leave max 2 items because the other items are for name space.
    if namespace.ns.len() > 2 {
        namespace.ns.drain(0..namespace.ns.len() - 2);
    }

    Some(DeclLocation {
        ns: tag_of_union.unwrap_or_else(|| key.clone()),
        key,
        name: namespace,
    })
}

// AOSP `ResolveName`: the innermost scope naming the first segment binds the whole name.
fn bind_type_name(name: &Namespace) -> Option<Namespace> {
    let (first, rest) = name.ns.split_first()?;
    let declared = |ns: &Namespace| DECLARATION_MAP.with(|map| map.borrow().contains_key(ns));
    let child = |scope: &Namespace| {
        let mut ns = scope.clone();
        ns.push(first);
        ns
    };
    let (top, imported, own) = DOCUMENT.with(|doc| {
        let doc = doc.borrow();
        let top = doc
            .package
            .as_ref()
            .map_or_else(Namespace::default, |p| Namespace::new(p, Namespace::AIDL));
        let imported = doc
            .imports
            .get(first)
            .map(|i| Namespace::new(i, Namespace::AIDL));
        (
            top,
            imported,
            doc.top_level.iter().any(|name| name == first),
        )
    });
    let with_rest = |mut key: Namespace| {
        key.ns.extend_from_slice(rest);
        key
    };

    // Enclosing types outward, then imports, then this document's own top-level types.
    let mut scope = current_namespace();
    let mut bound = None;
    while scope.ns.len() > top.ns.len() && scope.ns.starts_with(&top.ns) {
        let candidate = child(&scope);
        if declared(&candidate) {
            bound = Some(candidate);
            break;
        }
        scope.pop();
    }
    let bound = bound.or(imported).or_else(|| own.then(|| child(&top)));
    if let Some(key) = bound {
        return Some(with_rest(key)).filter(declared);
    }

    // As written (AOSP); then rsbinder's extension: another file's top-level type in this package.
    [child(&Namespace::default()), child(&top)]
        .into_iter()
        .map(with_rest)
        .find(declared)
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

// A constant or enum member; its value is folded once, in `owner`'s scope, under `key`.
#[derive(Clone)]
struct Symbol {
    key: String,
    owner: Namespace,
    def: SymbolDef,
}

#[derive(Clone)]
enum SymbolDef {
    Constant(ConstExpr),
    EnumExplicit(EnumMember, ConstExpr),
    // AOSP `previous + 1` fill: `offset` past the nearest valued member (or 0).
    EnumImplicit(EnumMember, Option<Box<Symbol>>, i64),
}

#[derive(Clone)]
struct EnumMember {
    enum_type: String,
    enum_name: String,
    member_name: String,
}

impl EnumMember {
    fn reference(&self, value: i64, kind: RefKind) -> ConstExpr {
        ConstExpr::new(ValueType::Reference {
            enum_type: self.enum_type.clone(),
            enum_name: self.enum_name.clone(),
            member_name: self.member_name.clone(),
            value,
            kind,
        })
    }
}

impl Symbol {
    fn label(&self) -> &str {
        match &self.def {
            SymbolDef::EnumExplicit(member, _) | SymbolDef::EnumImplicit(member, ..) => {
                &member.member_name
            }
            SymbolDef::Constant(_) => self.key.rsplit('.').next().unwrap_or(&self.key),
        }
    }

    // Every dependency is resolved by now, so the fold below reads values and never recurses.
    fn evaluate(&self) -> Result<ConstExpr, String> {
        in_scope(&self.owner, || match &self.def {
            SymbolDef::Constant(expr) => fold_symbol_expr(expr),
            SymbolDef::EnumExplicit(member, expr) => {
                let value = fold_symbol_expr(expr)?;
                // AOSP `AreCompatibleOperandTypes`: bool is integral; `decl_enum` rejects the rest.
                match RefKind::of(&value.value) {
                    Some(kind) => {
                        Ok(member.reference(value.to_i64().map_err(|e| e.message)?, kind))
                    }
                    None => Ok(value),
                }
            }
            SymbolDef::EnumImplicit(member, base, offset) => {
                let Some(base) = base else {
                    return Ok(member.reference(*offset, RefKind::Int32));
                };
                let base = resolve_symbol(base.as_ref().clone()).map_err(|e| e.message)?;
                match base.value {
                    // AOSP folds `previous + 1` at the promoted type and rejects overflow.
                    ValueType::Reference { value, kind, .. } => {
                        let long = kind == RefKind::Int64;
                        let kind = if long { RefKind::Int64 } else { RefKind::Int32 };
                        value
                            .checked_add(*offset)
                            .filter(|value| long || i32::try_from(*value).is_ok())
                            .map(|value| member.reference(value, kind))
                            .ok_or_else(|| {
                                format!(
                                    "constant expression computation overflows ('+' on {})",
                                    if long { "long" } else { "int" }
                                )
                            })
                    }
                    // A non-integral base is the diagnostic of every member counted from it.
                    _ => Ok(base),
                }
            }
        })
    }
}

// A name left unresolved in the owner's scope must not reach a referencer, where it could resolve.
fn fold_symbol_expr(expr: &ConstExpr) -> Result<ConstExpr, String> {
    let value = expr.calculate().map_err(|e| e.message)?;
    match value.value.unresolved_name() {
        Some(name) => Err(format!("cannot resolve constant reference '{name}'")),
        None => Ok(value),
    }
}

fn in_scope<R>(owner: &Namespace, f: impl FnOnce() -> R) -> R {
    let document_context = declaration_document_context(owner);
    let _document_guard = document_context.as_ref().map(DocumentGuard::new);
    let _ns_guard = NamespaceGuard::new(owner);
    f()
}

fn constant_symbol(owner: &Namespace, ident: &str, expr: &ConstExpr) -> Symbol {
    Symbol {
        key: format!("const {}.{ident}", owner.to_string(Namespace::AIDL)),
        owner: owner.clone(),
        def: SymbolDef::Constant(expr.clone()),
    }
}

fn enum_member_symbol(decl: &EnumDecl, ns: &Namespace, member_name: &str) -> Option<Symbol> {
    let index = decl
        .enumerator_list
        .iter()
        .position(|enumerator| enumerator.identifier == member_name)?;
    Some(enum_member_symbol_at(decl, ns, index))
}

fn enum_member_symbol_at(decl: &EnumDecl, ns: &Namespace, index: usize) -> Symbol {
    let enum_type = ns.to_string(Namespace::AIDL);
    let enumerator = &decl.enumerator_list[index];
    let member = EnumMember {
        enum_type: enum_type.clone(),
        enum_name: decl.name.clone(),
        member_name: enumerator.identifier.clone(),
    };
    let def = match &enumerator.const_expr {
        Some(expr) => SymbolDef::EnumExplicit(member, expr.clone()),
        None => {
            let base = decl.enumerator_list[..index]
                .iter()
                .rposition(|enumerator| enumerator.const_expr.is_some());
            let offset = (index - base.unwrap_or(0)) as i64;
            let base = base.map(|base| Box::new(enum_member_symbol_at(decl, ns, base)));
            SymbolDef::EnumImplicit(member, base, offset)
        }
    };
    Symbol {
        key: format!("enum {enum_type}.{}", enumerator.identifier),
        owner: ns.clone(),
        def,
    }
}

// What `ident` names inside `decl`; AOSP resolves references to constants only, never fields.
fn symbol_in_decl(decl: &Declaration, ns: &Namespace, ident: &str) -> Option<Symbol> {
    let constant = |var: &VariableDecl| {
        var.const_expr
            .as_ref()
            .filter(|_| var.constant && var.identifier == ident)
            .map(|expr| constant_symbol(ns, ident, expr))
    };
    // Direct members only: `Outer.X` never means `Outer.Inner.X`.
    let in_members = |members: &[Declaration]| {
        members.iter().find_map(|member| match member {
            Declaration::Variable(var) => constant(var),
            _ => None,
        })
    };
    match decl {
        Declaration::Variable(var) => constant(var),
        // `members` holds only nested type declarations; constants live in `constant_list`.
        Declaration::Interface(decl) => decl
            .constant_list
            .iter()
            .find(|var| var.identifier == ident)
            .and_then(|var| var.const_expr.as_ref())
            .map(|expr| constant_symbol(ns, ident, expr)),
        Declaration::Parcelable(decl) => in_members(&decl.members),
        Declaration::Enum(decl) => {
            enum_member_symbol(decl, ns, ident).or_else(|| in_members(&decl.members))
        }
        Declaration::Union(decl) => in_members(&decl.members),
    }
}

// AOSP `AidlConstantReference`: a type, then its member after the last `.`.
fn symbol_from_lookup(name: &str) -> Option<Symbol> {
    let (qualifier, ident) = name.rsplit_once('.')?;
    let found = locate_decl(qualifier, Namespace::AIDL)?;
    with_decl(&found.key, |decl| symbol_in_decl(decl, &found.ns, ident)).flatten()
}

// What a name in a constant expression refers to, from the current scope; `None` leaves a `Name`.
fn symbol_for_name(name: &str) -> Option<Symbol> {
    // A qualifier that is not a type is AOSP's "Failed to resolve"; no shorter form is tried.
    if name.contains('.') {
        return symbol_from_lookup(name);
    }

    // AOSP `AidlConstantReference::Resolve`: a bare name, a default's too, is in this type only.
    let curr = current_namespace();
    with_decl(&curr, |decl| symbol_in_decl(decl, &curr, name)).flatten()
}

// Bounds re-entrant `resolve_symbol` calls; a dependency chain itself never recurses.
const MAX_RESOLVE_NESTING: usize = 16;

fn circular_reference(name: &str) -> String {
    format!("circular reference detected while resolving constant '{name}'")
}

fn stored_value(key: &str) -> Option<Option<Result<ConstExpr, String>>> {
    CONST_VALUES.with(|values| values.borrow().get(key).cloned())
}

fn store_value(key: &str, value: Option<Result<ConstExpr, String>>) {
    CONST_VALUES.with(|values| {
        values.borrow_mut().insert(key.to_owned(), value);
    });
}

struct NestingGuard;

impl NestingGuard {
    fn enter() -> Option<Self> {
        RESOLVE_NESTING.with(|n| {
            (n.get() < MAX_RESOLVE_NESTING).then(|| {
                n.set(n.get() + 1);
                NestingGuard
            })
        })
    }
}

impl Drop for NestingGuard {
    fn drop(&mut self) {
        RESOLVE_NESTING.with(|n| n.set(n.get() - 1));
    }
}

struct Frame {
    symbol: Symbol,
    // Dependencies not yet checked, next last: (name as written, symbol).
    deps: Vec<(String, Symbol)>,
}

impl Frame {
    fn enter(symbol: Symbol) -> Self {
        store_value(&symbol.key, None);
        let mut deps: Vec<(String, Symbol)> = match &symbol.def {
            SymbolDef::Constant(expr) | SymbolDef::EnumExplicit(_, expr) => {
                in_scope(&symbol.owner, || {
                    expr.value
                        .referenced_names()
                        .into_iter()
                        .filter_map(|name| symbol_for_name(&name).map(|dep| (name, dep)))
                        .collect()
                })
            }
            SymbolDef::EnumImplicit(_, base, _) => base
                .iter()
                .map(|base| (base.label().to_owned(), base.as_ref().clone()))
                .collect(),
        };
        deps.reverse();
        Frame { symbol, deps }
    }
}

// Depth-first over an explicit stack: a chain costs no Rust stack; an in-progress dep is a cycle.
fn resolve_symbol(root: Symbol) -> Result<ConstExpr, ConstExprError> {
    match stored_value(&root.key) {
        Some(Some(value)) => return value.map_err(ConstExprError::new),
        Some(None) => return Err(ConstExprError::new(circular_reference(root.label()))),
        None => {}
    }
    let Some(_nesting) = NestingGuard::enter() else {
        return Err(ConstExprError::new(
            "constant expression nested too deeply (exceeded recursion limit)",
        ));
    };

    let root_key = root.key.clone();
    let mut stack = vec![Frame::enter(root)];
    while let Some(top) = stack.last_mut() {
        if let Some((name, dep)) = top.deps.pop() {
            match stored_value(&dep.key) {
                Some(Some(_)) => {}
                // A cycle has no value; AOSP rejects it ("Found a circular reference").
                Some(None) => {
                    let key = top.symbol.key.clone();
                    stack.pop();
                    store_value(&key, Some(Err(circular_reference(&name))));
                }
                None => stack.push(Frame::enter(dep)),
            }
            continue;
        }
        if let Some(frame) = stack.pop() {
            let value = frame.symbol.evaluate();
            store_value(&frame.symbol.key, Some(value));
        }
    }

    match stored_value(&root_key) {
        Some(Some(value)) => value.map_err(ConstExprError::new),
        _ => Err(ConstExprError::new(circular_reference(&root_key))),
    }
}

/// Final value of what `name` refers to here; `Ok(None)` leaves it a `Name`.
pub(crate) fn name_to_const_expr(name: &str) -> Result<Option<ConstExpr>, ConstExprError> {
    symbol_for_name(name).map(resolve_symbol).transpose()
}

/// The discriminant of `member_name` in the enum `lookup_decl` holds.
pub(crate) fn enum_member_value(
    lookup_decl: &LookupDecl,
    member_name: &str,
) -> Result<ConstExpr, ConstExprError> {
    let symbol = match &lookup_decl.decl {
        Declaration::Enum(decl) => enum_member_symbol(decl, &lookup_decl.ns, member_name),
        _ => None,
    };
    let symbol = symbol
        .ok_or_else(|| ConstExprError::new(format!("'{member_name}' is not an enum member")))?;
    resolve_symbol(symbol)
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

// The interface template declares `__Rsb*` names where signatures resolve (`RESERVED_NAME_PREFIX`).
fn reject_reserved_type_name(
    name: &str,
    role: &str,
    span: &pest::Span<'_>,
) -> Result<(), AidlError> {
    let simple = name.rsplit('.').next().unwrap_or(name);
    if !simple.starts_with(crate::generator::RESERVED_NAME_PREFIX) {
        return Ok(());
    }
    Err(make_parse_error(
        format!(
            "{role} '{simple}' starts with '{}', which is reserved for names the generated \
             code declares",
            crate::generator::RESERVED_NAME_PREFIX
        ),
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
    // AOSP `AidlDocument::DefinedTypes()`: this document's own top-level type names.
    top_level: Vec<String>,
}

impl DocumentContext {
    fn from_document(document: &Document) -> Self {
        Self {
            package: document.package.clone(),
            imports: document.imports.clone(),
            top_level: document
                .decls
                .iter()
                .filter(|decl| decl.is_variable().is_none())
                .map(|decl| decl.name().to_owned())
                .collect(),
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

    /// Union variant: first letter uppercased (AOSP GetCapitalizedName, aidl_language.cpp:998).
    pub fn union_identifier(&self) -> String {
        let mut name = self.identifier.clone();
        if let Some(first) = name.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        name
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
            .direction_at(&self.direction, self.direction_span, &self.identifier)?
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

    /// The type declarations nested directly in this one; a variable has none.
    pub(crate) fn nested_types(&self) -> impl DoubleEndedIterator<Item = &Declaration> {
        let members: &[Declaration] = match self {
            Declaration::Parcelable(decl) => &decl.members,
            Declaration::Interface(decl) => &decl.members,
            Declaration::Enum(decl) => &decl.members,
            Declaration::Union(decl) => &decl.members,
            Declaration::Variable(_) => &[],
        };
        members
            .iter()
            .filter(|member| member.is_variable().is_none())
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

fn parse_unary(mut pairs: pest::iterators::Pairs<Rule>) -> Result<(ConstExpr, usize), AidlError> {
    let op = pairs.next().unwrap();
    let operator = op.as_str().to_owned();
    let (factor, depth) = parse_factor(pairs.next().unwrap().into_inner().next().unwrap())?;
    let depth = bound_expr_depth(depth + 1, &op)?;
    Ok((ConstExpr::new_unary(&operator, factor), depth))
}

// Bounds the built tree, whatever shape the pre-scan missed: its clone/drop recurse per level.
fn bound_expr_depth(depth: usize, op: &pest::iterators::Pair<Rule>) -> Result<usize, AidlError> {
    if depth > MAX_OPERATOR_RUN {
        let name = CURRENT_SOURCE_NAME.with(|n| n.borrow().clone());
        let text = CURRENT_SOURCE_TEXT.with(|t| t.borrow().clone());
        return Err(ParseError::nesting_too_deep(
            &name,
            &text,
            op.as_span().start(),
            NestingLimit::OperatorRun.describe(),
            MAX_OPERATOR_RUN,
        )
        .into());
    }
    Ok(depth)
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

    // AOSP ParseIntegral: u32/u64 pin the size for decimal only; hex ignores them below.
    if is_u32 && radix == 10 {
        let parsed_value = u32::from_str_radix(value, radix).map_err(|err| {
            make_parse_error(
                format!("invalid u32 literal '{arg_value}': {err}"),
                span.0,
                span.1,
            )
        })?;
        return Ok(ConstExpr::new(ValueType::Int32(parsed_value as i32 as _)));
    }
    if is_u64 && radix == 10 {
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

fn parse_factor(pair: pest::iterators::Pair<Rule>) -> Result<(ConstExpr, usize), AidlError> {
    match pair.as_rule() {
        Rule::expression => parse_expression_with_depth(pair.into_inner()),
        Rule::unary => parse_unary(pair.into_inner()),
        Rule::value => Ok((parse_value(pair.into_inner().next().unwrap())?, 0)),
        _ => unreachable!("Unexpected rule in parse_factor(): {}", pair),
    }
}

fn parse_expression_term(
    pair: pest::iterators::Pair<Rule>,
) -> Result<(ConstExpr, usize), AidlError> {
    match pair.as_rule() {
        Rule::equality
        | Rule::comparison
        | Rule::bitwise_or
        | Rule::bitwise_xor
        | Rule::bitwise_and
        | Rule::shift
        | Rule::arith
        | Rule::logical_or
        | Rule::logical_and => parse_expression_with_depth(pair.into_inner()),
        Rule::factor => parse_factor(pair.into_inner().next().unwrap()),
        _ => unreachable!("Unexpected rule in Rule::parse_expression_into: {}", pair),
    }
}

fn parse_expression(pairs: pest::iterators::Pairs<Rule>) -> Result<ConstExpr, AidlError> {
    parse_expression_with_depth(pairs).map(|(expr, _)| expr)
}

fn parse_expression_with_depth(
    mut pairs: pest::iterators::Pairs<Rule>,
) -> Result<(ConstExpr, usize), AidlError> {
    let (mut lhs, mut depth) = parse_expression_term(pairs.next().unwrap())?;

    while let Some(pair) = pairs.next() {
        let op = pair.as_str().to_owned();
        let (rhs, rhs_depth) = parse_expression_term(pairs.next().unwrap())?;
        depth = bound_expr_depth(depth.max(rhs_depth) + 1, &pair)?;

        lhs = ConstExpr::new_expr(lhs, &op, rhs)
    }

    Ok((lhs, depth))
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
                reject_reserved_type_name(pair.as_str(), "interface name", &span)?;
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
                reject_reserved_type_name(pair.as_str(), "parcelable name", &span)?;
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
                reject_reserved_type_name(pair.as_str(), "enum name", &span)?;
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
                reject_reserved_type_name(pair.as_str(), "union name", &span)?;
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
    if let Declaration::Union(union) = decl {
        let mut tag_ns = namespace.clone();
        tag_ns.push("Tag");
        // AOSP `UnionTagGenerater` (parser.cpp): one unvalued enumerator per field, in order.
        let enumerator_list = union
            .members
            .iter()
            .filter_map(Declaration::is_variable)
            .filter(|var| !var.constant)
            .map(|var| Enumerator {
                identifier: var.identifier.clone(),
                ..Default::default()
            })
            .collect();
        let tag_enum = Declaration::Enum(EnumDecl {
            namespace: tag_ns.clone(),
            name: "Tag".into(),
            enumerator_list,
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

/// Operators on one operand path, brackets included: each is one ConstExpr tree level.
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
            // The scan cannot tell `a < b` from an unclosed `List<b` until a `>` or `;`.
            NestingLimit::Generic => {
                "generic types are nested too deeply, or one statement has too many `<` \
                 before a name (a comparison counts)"
            }
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
    let mut op_run: usize = 0; // operators on the current operand path, inherited into brackets
    let mut run_base: Vec<usize> = Vec::new(); // `op_run` at each open bracket
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
            // A close bracket resumes the enclosing run; an element/statement restarts at its base.
            b'(' | b'[' | b'{' => {
                bracket_depth += 1;
                run_base.push(op_run);
            }
            b')' | b']' | b'}' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                op_run = run_base.pop().unwrap_or(0);
            }
            b',' => op_run = run_base.last().copied().unwrap_or(0),
            // `<<` shift / `<=` are not generic openers.
            b'<' if matches!(next(i), Some(b'<') | Some(b'=')) => {
                op_run += 1; // `<<`/`<=` drives one binary-operator level
                i += 2;
                continue;
            }
            b'<' => {
                // A type argument starts with a name, an annotation or a comment; a digit cannot.
                let next_tok = bytes[i + 1..].iter().find(|b| !b.is_ascii_whitespace());
                if next_tok.is_some_and(|b| b.is_ascii_alphabetic() || b"_@/".contains(b)) {
                    angle_open.push(i);
                } else {
                    op_run += 1;
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
                op_run += 1;
                i += 2;
                continue;
            }
            b'>' if !angle_open.is_empty() => {
                generic_depth = generic_depth.max(angle_open.len());
                angle_open.pop();
            }
            b'>' => op_run += 1,
            b';' => {
                angle_open.clear();
                op_run = run_base.last().copied().unwrap_or(0);
            }
            // Each operator is one tree level; `op_run` bounds the deepest operand path.
            b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^' | b'!' | b'~' | b'=' => {
                op_run += 1
            }
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
    reset_const_values();
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
    // Values folded while declarations were still missing (annotation parameters) are not final.
    reset_const_values();

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
        *doc.borrow_mut() = DocumentContext::default();
    });
    CURRENT_WARNINGS.with(|w| {
        w.borrow_mut().clear();
    });
    BUILTIN_RUST_PATHS.with(|map| {
        map.borrow_mut().clear();
    });
    reset_const_values();
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
    fn test_hex_unsigned_suffix_follows_aosp_parse_integral() -> Result<(), Box<dyn Error>> {
        // AOSP ParseIntegral: hex tries u32 (as INT32) then u64 unless suffixed u8 or l/L.
        assert_eq!(
            parse_intvalue("0xFFFFFFFFu64", (0, 0))?.value,
            ValueType::Int32(-1)
        );
        assert_eq!(parse_intvalue("0x1u64", (0, 0))?.value, ValueType::Int32(1));
        assert_eq!(
            parse_intvalue("0x1FFFFFFFFu32", (0, 0))?.value,
            ValueType::Int64(0x1FFFFFFFF)
        );
        assert_eq!(
            parse_intvalue("0xFFFFFFFFL", (0, 0))?.value,
            ValueType::Int64(0xFFFFFFFF)
        );
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
