// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Clippy's `missing_const_for_thread_local` is a false positive for our
// `HashMap` / `HashSet` / `Document` (transitively HashMap) initializers
// — they are not `const fn` (need `RandomState` at runtime), so the
// suggested `const { ... }` wrap fails with E0015. The lint fires against
// the whole `thread_local!` block, and neither macro-invocation-level nor
// per-static `#[allow]` reaches that scope, so suppression is file-scope.
#![allow(clippy::missing_const_for_thread_local)]

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

#[derive(Debug, Clone)]
pub enum SymbolType {
    EnumMember,
    InterfaceConstant,
    // Future expansion: ParcelableDefault, Variable, etc.
}

#[derive(Debug, Clone)]
pub(crate) struct Symbol {
    pub value: crate::const_expr::ConstExpr,
    pub symbol_type: SymbolType,
}

thread_local! {
    static DECLARATION_MAP: RefCell<HashMap<Namespace, Declaration>> = RefCell::new(HashMap::new());
    static DECLARATION_DOCUMENT_MAP: RefCell<HashMap<Namespace, DocumentContext>> = RefCell::new(HashMap::new());
    static NAMESPACE_STACK: RefCell<Vec<Namespace>> = const { RefCell::new(Vec::new()) };
    static DOCUMENT: RefCell<Document> = RefCell::new(Document::new());

    // Universal Symbol Table - supports all types of named constants
    static SYMBOL_TABLE: RefCell<HashMap<String, Symbol>> = RefCell::new(HashMap::new());
    static ENUM_VALUE_CACHE: RefCell<HashMap<String, ConstExpr>> = RefCell::new(HashMap::new());
    static ENUM_RESOLUTION_STACK: RefCell<HashSet<String>> = RefCell::new(HashSet::new());

    // Filename and source text of the source currently being parsed (used for error message generation)
    static CURRENT_SOURCE_NAME: RefCell<String> = const { RefCell::new(String::new()) };
    static CURRENT_SOURCE_TEXT: RefCell<String> = const { RefCell::new(String::new()) };

    // Non-fatal diagnostics accumulated during the current `parse_document`
    // call. Drained into `Document::warnings` before parse_document returns.
    static CURRENT_WARNINGS: RefCell<Vec<crate::error::AidlWarning>> = const { RefCell::new(Vec::new()) };
}

/// AOSP-recognised AIDL annotations (`aidl_language.cpp::AidlAnnotation::AllSchemas()`,
/// 23 entries as of android-16). Annotations outside this set are
/// surfaced as `cargo:warning=...` so typos and unsupported annotations
/// don't silently disappear. Sorted alphabetically for grep-ability.
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
        SourceGuard
    }
}

impl Drop for SourceGuard {
    fn drop(&mut self) {
        CURRENT_SOURCE_NAME.with(|name| name.borrow_mut().clear());
        CURRENT_SOURCE_TEXT.with(|text| text.borrow_mut().clear());
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

    // 1, check if the type exists in the current namespace.
    let mut curr_ns = current_namespace();
    ns_vec.append(&mut make_ns_candidate(&curr_ns, &namespace));

    curr_ns.pop(); // For parent namespace
    ns_vec.append(&mut make_ns_candidate(&curr_ns, &namespace));

    // 2. check if the type exists in the imports from the current document.
    DOCUMENT.with(|curr_doc| {
        let curr_doc = curr_doc.borrow();

        if let Some(package) = &curr_doc.package {
            let package_ns = Namespace::new(package, Namespace::AIDL);
            ns_vec.append(&mut make_ns_candidate(&package_ns, &namespace));
        }

        if let Some(imported) = curr_doc.imports.get(&namespace.ns[0]) {
            let mut new_ns = Namespace::new(imported, Namespace::AIDL);
            new_ns.ns.extend_from_slice(&namespace.ns[1..]);
            ns_vec.push(new_ns);
        }
    });

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

        let curr_ns = current_namespace();
        if let Some(decl) = hashmap.borrow().get(&curr_ns) {
            return Some((decl.clone(), curr_ns));
        }

        None
    })?;

    // Synthetic union-tag enums (`EnumDecl::tag_of_union`) record the
    // parent union's namespace as their codegen-effective module path
    // — the `Tag` struct is a sibling inside `mod <Union>`, not its
    // own `mod Tag`, so the default `<ns>::<name>` doubling needs to
    // resolve against the union's ns to emit `<Union>::Tag` instead
    // of `<Union>::Tag::Tag`.
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

fn make_const_expr(const_expr: Option<&ConstExpr>, lookup_decl: &LookupDecl) -> ConstExpr {
    if let Some(expr) = const_expr {
        expr.clone()
    } else {
        let ns = current_namespace().relative_mod(&lookup_decl.ns);

        let name = if !ns.is_empty() {
            format!(
                "{}{}{}",
                ns,
                Namespace::RUST,
                lookup_decl.name.to_string(Namespace::RUST)
            )
        } else {
            lookup_decl.name.to_string(Namespace::RUST)
        };
        ConstExpr::new(ValueType::Name(name))
    }
}

fn lookup_name_from_decl(decl: &Declaration, lookup_decl: &LookupDecl) -> Option<ConstExpr> {
    let lookup_ident = lookup_decl.name.ns.last().unwrap().to_owned();
    match decl {
        Declaration::Variable(decl) => {
            if decl.identifier == lookup_ident {
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
            lookup_name_members(&decl.members, lookup_decl)
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

fn lookup_name_members(members: &Vec<Declaration>, lookup_decl: &LookupDecl) -> Option<ConstExpr> {
    for decl in members {
        if let Some(expr) = lookup_name_from_decl(decl, lookup_decl) {
            return Some(expr);
        }
    }
    None
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

    // Resolve inside the enum declaration, not through a global member name.
    for enumerator in &enum_decl.enumerator_list {
        // A genuinely non-integral explicit value (String/Array — not an
        // unresolved Name) must not be swallowed into the auto-increment
        // counter: that would fabricate a wrong, silently-zeroed wire
        // discriminant. Carry the raw value out so `decl_enum`'s `to_i64()`
        // guard surfaces the diagnostic.
        let mut non_integral: Option<ConstExpr> = None;
        if let Some(const_expr) = &enumerator.const_expr {
            if let Ok(calculated) = const_expr.calculate() {
                if !matches!(calculated.value, ValueType::Name(_)) {
                    match calculated.to_i64() {
                        Ok(v) => enum_val = v,
                        Err(_) => non_integral = Some(calculated),
                    }
                }
            }
        }

        if enumerator.identifier == member_name {
            result = Some(non_integral.unwrap_or_else(|| {
                ConstExpr::new(ValueType::Reference {
                    enum_type: enum_type.clone(),
                    enum_name: enum_decl.name.clone(),
                    member_name: member_name.to_string(),
                    value: enum_val,
                })
            }));
            break;
        }

        // `wrapping_add` matches AOSP's C++ wraparound semantics and avoids
        // a debug-build panic / release silent overflow when an enumerator
        // carries an explicit `i64::MAX` value followed by an auto-increment
        // member.
        enum_val = enum_val.wrapping_add(1);
    }

    ENUM_RESOLUTION_STACK.with(|stack| {
        stack.borrow_mut().remove(&resolution_key);
    });

    if let Some(expr) = &result {
        ENUM_VALUE_CACHE.with(|cache| {
            cache.borrow_mut().insert(resolution_key, expr.clone());
        });
    }

    result
}

pub fn name_to_enum_member_const_expr(name: &str, target_enum: Option<&str>) -> Option<ConstExpr> {
    // A field default has a target type. Use it to resolve unqualified
    // members and reject defaults from a different enum family.
    if let Some((enum_name, member_name)) = name.rsplit_once('.') {
        let lookup_decl = lookup_decl_from_name(enum_name, Namespace::AIDL)?;
        if !matches!(lookup_decl.decl, Declaration::Enum(_)) {
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

// Universal symbol registration - supports all types of named constants
pub fn register_symbol(
    name: &str,
    value: ConstExpr,
    symbol_type: SymbolType,
    namespace: Option<&str>,
) {
    let symbol = Symbol { value, symbol_type };

    SYMBOL_TABLE.with(|table| {
        let mut table = table.borrow_mut();

        match &symbol.symbol_type {
            SymbolType::EnumMember => {
                // Enum members are only registered under their enum type.
                // Simple enum member names are not globally unique.
                if let Some(ns) = namespace {
                    let qualified_name = format!("{}.{}", ns, name);
                    table.insert(qualified_name, symbol);
                }
            }
            _ => {
                // Register with simple name
                table.insert(name.to_string(), symbol.clone());

                // Also register with qualified name if namespace is provided
                if let Some(ns) = namespace {
                    let qualified_name = format!("{}.{}", ns, name);
                    table.insert(qualified_name, symbol);
                }
            }
        }
    });
}

// Note: register_enum_member removed as it's not used
// Use register_symbol directly with SymbolType::EnumMember

// Enhanced name resolution with universal symbol table
pub fn name_to_const_expr(name: &str) -> Option<ConstExpr> {
    if let Some(expr) = name_to_enum_member_const_expr(name, None) {
        return Some(expr);
    }

    // First, try to resolve from universal symbol table (exact match)
    let symbol_result =
        SYMBOL_TABLE.with(|table| table.borrow().get(name).map(|symbol| symbol.value.clone()));

    if symbol_result.is_some() {
        return symbol_result;
    }

    // For dotted names, try namespace-aware declaration lookup before variant stripping.
    // This ensures that qualified names like "ParcelableWithNested.Status.OK"
    // are resolved with full namespace context rather than being stripped to
    // shorter variants that may lose parent type information.
    if name.contains('.') {
        if let Some(lookup_decl) = lookup_decl_from_name(name, Namespace::AIDL) {
            if let Some(expr) = lookup_name_from_decl(&lookup_decl.decl, &lookup_decl) {
                return Some(expr);
            }
        }
    }

    // Try alternative name formats for cross-references
    let alternative_formats = generate_name_variants(name);
    for variant in alternative_formats {
        let variant_result = SYMBOL_TABLE.with(|table| {
            table
                .borrow()
                .get(&variant)
                .map(|symbol| symbol.value.clone())
        });
        if variant_result.is_some() {
            return variant_result;
        }
    }

    // Fallback to original resolution
    if let Some(lookup_decl) = lookup_decl_from_name(name, Namespace::AIDL) {
        return lookup_name_from_decl(&lookup_decl.decl, &lookup_decl);
    }

    None
}

// Generate possible name variants for flexible resolution
fn generate_name_variants(name: &str) -> Vec<String> {
    let mut variants = Vec::new();

    // Handle dot notation: "A.B.C" -> ["A.B.C", "B.C", "C"]
    // Try progressively shorter prefixes to find the best qualified match
    if name.contains('.') {
        let parts: Vec<&str> = name.split('.').collect();
        for i in 0..parts.len() {
            variants.push(parts[i..].join("."));
        }
    } else {
        // For simple names, try with current namespace context
        let current_ns = current_namespace().to_string(crate::Namespace::AIDL);

        if !current_ns.is_empty() {
            variants.push(format!("{}.{}", current_ns, name));
        }
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
}

impl VariableDecl {
    pub fn identifier(&self) -> String {
        self.identifier.to_owned()
    }

    pub fn const_identifier(&self) -> String {
        self.identifier.to_uppercase()
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
    pub type_params: Vec<String>,
    pub cpp_header: String,
    pub ndk_header: String,
    pub rust_type: String,
    pub members: Vec<Declaration>,
    // pub name_dict: Option<HashMap<String, ConstExpr>>,
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
    pub fn to_value_type(&self) -> Result<ValueType, crate::error::AidlError> {
        let generator = match self {
            Generic::Type1 {
                type_args1,
                non_array_type: _,
                type_args2: _,
            } => type_generator::TypeGenerator::new_with_type(&type_args1[0])?,
            Generic::Type2 {
                non_array_type,
                type_args: _,
            } => type_generator::TypeGenerator::new(non_array_type)?,
            Generic::Type3 { type_args } => {
                type_generator::TypeGenerator::new_with_type(&type_args[0])?
            }
        };

        Ok(generator.value_type)
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
    JavaOnly,
    VintfStability,
}

/// Returns whether the annotation list contains the queried annotation.
pub fn has_annotation(annotation_list: &[Annotation], query_type: AnnotationType) -> bool {
    annotation_list.iter().any(|annotation| match query_type {
        AnnotationType::VintfStability => annotation.annotation == "@VintfStability",
        AnnotationType::IsNullable => annotation.annotation == "@nullable",
        AnnotationType::JavaOnly => annotation.annotation.starts_with("@JavaOnly"),
    })
}

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

/// Walks an `Annotation::const_expr` (or array element) and returns the
/// owned string when the value is `ValueType::String(_)`. The pest
/// grammar already strips the surrounding double quotes, so no
/// `trim_matches('"')` is required.
fn const_expr_as_string(expr: &ConstExpr) -> Option<String> {
    if let crate::const_expr::ValueType::String(ref s) = expr.value {
        Some(s.clone())
    } else {
        None
    }
}

/// Walks an `Annotation::const_expr` representing an AIDL string-array
/// literal (`{"A", "B"}`) and returns the contained strings. Returns
/// `None` for non-array values or arrays containing non-string elements
/// — both are AOSP-rejected per
/// `aidl_language.cpp::AidlAnnotation::CheckValid()`.
fn const_expr_as_string_array(expr: &ConstExpr) -> Option<Vec<String>> {
    let crate::const_expr::ValueType::Array(items) = &expr.value else {
        return None;
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(const_expr_as_string(item)?);
    }
    Some(out)
}

/// Extracts a parsed `@EnforcePermission(...)` from a method's annotation
/// list, or `None` when the annotation is absent or syntactically
/// malformed (no `value`/`allOf`/`anyOf` recognized — already surfaced by
/// `parse_annotation_list`'s unknown-annotation warning path).
///
/// AOSP schema reference: `aidl_language.cpp:211-214` declares
/// `EnforcePermission` with `{{"value", kStringType}, {"anyOf",
/// kStringArrayType}, {"allOf", kStringArrayType}}` — exactly the three
/// forms decoded here.
pub fn enforce_permission_from_annotation_list(
    annotation_list: &[Annotation],
    method_name: &str,
) -> Result<Option<EnforcePermissionExpr>, crate::error::AidlError> {
    for annotation in annotation_list {
        if annotation.annotation != "@EnforcePermission" {
            continue;
        }

        // Shorthand `@EnforcePermission("X")`: AIDL grammar parses the
        // bare positional argument into `annotation.const_expr` rather
        // than `parameter_list`.
        if let Some(c) = &annotation.const_expr {
            if let Some(s) = const_expr_as_string(c) {
                return Ok(Some(EnforcePermissionExpr::Single(s)));
            }
        }

        // Named-parameter forms — match the AOSP schema's three keys.
        for param in &annotation.parameter_list {
            match param.identifier.as_str() {
                "value" => {
                    if let Some(s) = const_expr_as_string(&param.const_expr) {
                        return Ok(Some(EnforcePermissionExpr::Single(s)));
                    }
                }
                "allOf" => {
                    if let Some(items) = const_expr_as_string_array(&param.const_expr) {
                        return Ok(Some(EnforcePermissionExpr::AllOf(items)));
                    }
                }
                "anyOf" => {
                    if let Some(items) = const_expr_as_string_array(&param.const_expr) {
                        return Ok(Some(EnforcePermissionExpr::AnyOf(items)));
                    }
                }
                _ => {}
            }
        }

        // `@EnforcePermission` annotation present but no recognized
        // argument form matched. Refuse to emit an unguarded Bn —
        // AOSP rejects this at build time too.
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

pub fn get_descriptor_from_annotation_list(annotation_list: &Vec<Annotation>) -> Option<String> {
    for annotation in annotation_list {
        if annotation.annotation == "@Descriptor" {
            for param in &annotation.parameter_list {
                if param.identifier == "value" {
                    return Some(param.const_expr.to_value_string());
                }
            }
        }
    }

    None
}

pub fn get_backing_type(
    annotation_list: &Vec<Annotation>,
    name_span: Option<(usize, usize)>,
) -> Result<type_generator::TypeGenerator, crate::error::AidlError> {
    // parse "@Backing(type="byte")"
    for annotation in annotation_list {
        if annotation.annotation == "@Backing" {
            for param in &annotation.parameter_list {
                if param.identifier == "type" {
                    let type_name: String =
                        param.const_expr.to_value_string().trim_matches('"').into();

                    // AOSP allows only {byte, int, long} as enum backing types.
                    // See aidl_language.cpp::AidlEnumDeclaration::Autofill().
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
/// Mirrors `make_parse_error` for the semantic-error family.
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

/// AOSP `aidl_language.cpp:1211` rejects oneway methods that return a
/// value or carry `out`/`inout` parameters — oneway is fire-and-forget,
/// so reply data has nowhere to go. Mirror that here so the diagnostic
/// surfaces at parse time rather than as a confusing codegen / wire
/// mismatch later. A method is "oneway" if either its own `oneway`
/// keyword or its enclosing interface's `oneway` keyword is set
/// (interface-level `oneway` propagates to every method).
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

    // Strip the integer suffix. AOSP accepts u8 / u32 / u64 / l / L; check
    // the multi-character unsigned suffixes before the single-char `l`/`L`.
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

    // AOSP permits `_` digit separators (e.g. `1_000_000`, `0xFF_FF`);
    // Rust's `from_str_radix` rejects them, so strip them before parsing.
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
    // println!("parse_factor {:?}", pair);
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

fn parse_string_term(pair: pest::iterators::Pair<Rule>) -> Result<ConstExpr, AidlError> {
    match pair.as_rule() {
        Rule::C_STR => {
            let span = pair.as_span();
            let raw = pair.as_str();
            let inner = &raw[1..raw.len() - 1];
            // The string is emitted verbatim into a generated Rust `"..."`.
            // rsbinder intentionally allows non-ASCII (UTF-8) here — e.g.
            // `const String MSG = "한글";` — because it round-trips as a valid
            // Rust string literal (more lenient than AOSP `isValidLiteralChar`,
            // which rejects non-ASCII). But a control byte or a backslash would
            // be emitted verbatim and fail to compile (a raw `\X` is not
            // necessarily a valid Rust escape; rsbinder does not decode string
            // escapes). Reject only those at parse time.
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
        Rule::qualified_name => Ok(ConstExpr::new(ValueType::Name(pair.as_str().into()))),
        _ => unreachable!("Unexpected rule in Rule::parse_string_term: {}", pair),
    }
}

fn parse_string_expr(pairs: pest::iterators::Pairs<Rule>) -> Result<ConstExpr, AidlError> {
    let mut expr: Option<ConstExpr> = None;

    for pair in pairs {
        match pair.as_rule() {
            Rule::string_term => {
                let term = parse_string_term(pair.into_inner().next().unwrap())?;
                expr = match expr {
                    Some(expr) => Some(ConstExpr::new_expr(expr, "+", term)),
                    None => Some(term),
                }
            }
            _ => unreachable!("Unexpected rule in Rule::parse_string_expr: {}", pair),
        }
    }

    Ok(expr.expect("internal: empty string_expr"))
}

fn parse_const_expr(pair: pest::iterators::Pair<Rule>) -> Result<ConstExpr, AidlError> {
    match pair.as_rule() {
        Rule::constant_value_list => {
            let mut value_list = Vec::new();
            for pair in pair.into_inner() {
                match pair.as_rule() {
                    Rule::const_expr => {
                        // An empty `{}` parses to a `const_expr` with no
                        // inner pair; reject it with a diagnostic rather
                        // than `unwrap()`-panicking on user input.
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
                // Map the supported C/AIDL escape sequences to their actual code
                // points (e.g. `'\n'` becomes newline, not the literal 'n').
                // rsbinder intentionally supports these (more lenient than AOSP,
                // which only allows `'\0'`). An unrecognized escape used to fall
                // through to the post-backslash char verbatim — silently
                // producing the wrong code point (`'\a'` -> 'a' = 97, not bell).
                // Reject it instead.
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

        Rule::string_expr => parse_string_expr(pair.into_inner()),

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
    let mut list = Vec::new();
    for pair in pairs {
        list.push(parse_parameter(pair.into_inner())?);
    }

    Ok(list)
}

fn parse_annotation(pairs: pest::iterators::Pairs<Rule>) -> Result<Annotation, AidlError> {
    // `annotation_span` is set by the caller (`parse_annotation_list`) from the
    // outer `annotation` rule's pair span so the diagnostic label naturally
    // covers the whole `@Foo(...)` form, including parens that pest's child
    // rules (ANNOTATION / const_expr / parameter_list) do not span.
    let mut annotation = Annotation::default();
    for pair in pairs {
        match pair.as_rule() {
            Rule::ANNOTATION => {
                annotation.annotation = pair.as_str().into();
            }

            Rule::const_expr => {
                let span = pair.as_span();
                match pair.into_inner().next() {
                    Some(inner) => annotation.const_expr = Some(parse_const_expr(inner)?),
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

fn parse_annotation_list(
    pairs: pest::iterators::Pairs<Rule>,
) -> Result<Vec<Annotation>, AidlError> {
    let mut annotation_list = Vec::new();
    for pair in pairs {
        // Capture the outer `annotation` rule's span (covers `@Foo(...)` or
        // bare `@Foo`) before descending into the inner pairs.
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
        }

        annotation_list.push(annotation);
    }

    Ok(annotation_list)
}

fn parse_type_args(pairs: pest::iterators::Pairs<Rule>) -> Result<Vec<Type>, AidlError> {
    let mut res = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::r#type => res.push(parse_type(pair.into_inner())?),
            _ => unreachable!("Unexpected rule in parse_type_args(): {}", pair),
        }
    }

    Ok(res)
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
                    non_array_type: parse_non_array_type(pairs.next().unwrap().into_inner())?,
                    type_args2: parse_type_args(pairs.next().unwrap().into_inner())?,
                };

                non_array_type.generic = Some(Box::new(generic));
            }

            Rule::generic_type2 => {
                let mut pairs = pair.into_inner();
                let generic = Generic::Type2 {
                    non_array_type: parse_non_array_type(pairs.next().unwrap().into_inner())?,
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
                decl.identifier = pair.as_str().into();
            }
            Rule::const_expr => match pair.into_inner().next() {
                Some(pair) => decl.const_expr = Some(parse_const_expr(pair)?),
                None => decl.const_expr = None,
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
                let expr = parse_intvalue(pair.as_str(), (span.start(), span.end()))?
                    .calculate()
                    .map_err(|e| make_parse_error(e.message, span.start(), span.end()))?;
                decl.intvalue = Some(match expr.value {
                    ValueType::Byte(v) => v as _,
                    ValueType::Int32(v) => v as _,
                    ValueType::Int64(v) => v,
                    _ => unreachable!(
                        "Unexpected Expression in parse_method_decl(): {}, \"{}\"",
                        pair,
                        pair.as_str()
                    ),
                });
                decl.intvalue_span = Some((span.start(), span.end()));
            }
            _ => unreachable!(
                "Unexpected rule in parse_method_decl(): {}, \"{}\"",
                pair,
                pair.as_str()
            ),
        }
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
                interface
                    .method_list
                    .push(parse_method_decl(pair.into_inner())?);
            }

            Rule::constant_decl => {
                interface
                    .constant_list
                    .push(parse_variable_decl(pair.into_inner(), true)?);
            }

            Rule::interface_members => {
                parse_interface_members(pair.into_inner(), interface)?;
            }

            Rule::decl => {
                interface
                    .members
                    .append(&mut parse_decl(pair.into_inner())?);
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
            Rule::variable_decl => {
                res.push(Declaration::Variable(parse_variable_decl(
                    pair.into_inner(),
                    false,
                )?));
            }
            Rule::constant_decl => {
                res.push(Declaration::Variable(parse_variable_decl(
                    pair.into_inner(),
                    true,
                )?));
            }
            Rule::decl => res.append(&mut parse_decl(pair.into_inner())?),
            _ => unreachable!("Unexpected rule in parse_parcelable_members(): {}", pair),
        }
    }

    Ok(res)
}

fn parse_optional_type_params(pairs: pest::iterators::Pairs<Rule>) -> Vec<String> {
    let mut res = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::identifier => res.push(pair.as_str().into()),
            _ => unreachable!("Unexpected rule in parse_optional_type_params(): {}", pair),
        }
    }

    res
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
    pairs: pest::iterators::Pairs<Rule>,
) -> Result<Declaration, AidlError> {
    let mut parcelable = ParcelableDecl {
        annotation_list,
        ..Default::default()
    };

    for pair in pairs {
        match pair.as_rule() {
            Rule::qualified_name => {
                parcelable.name = pair.as_str().into();
            }

            Rule::optional_type_params => {
                parcelable.type_params = parse_optional_type_params(pair.into_inner());
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

    Ok(Declaration::Parcelable(parcelable))
}

#[derive(Debug, Default, Clone)]
pub struct Enumerator {
    pub identifier: String,
    pub const_expr: Option<ConstExpr>,
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
}

fn parse_enumerator(pairs: pest::iterators::Pairs<Rule>) -> Result<Enumerator, AidlError> {
    let mut res = Enumerator::default();

    for pair in pairs {
        match pair.as_rule() {
            Rule::identifier => {
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
                enum_decl.name = pair.as_str().into();
                enum_decl.name_span = Some((span.start(), span.end()));
            }
            Rule::enumerator => enum_decl
                .enumerator_list
                .push(parse_enumerator(pair.into_inner())?),
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
    pub type_params: Vec<String>,
    pub members: Vec<Declaration>,
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
                union_decl.name = pair.as_str().into();
                union_decl.name_span = Some((span.start(), span.end()));
            }
            Rule::optional_type_params => {
                union_decl.type_params = parse_optional_type_params(pair.into_inner());
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

    // Implicit nested `Tag` enum for unions (AIDL semantics). The
    // union codegen template already emits a `Tag` struct inside the
    // union's module; this stub `EnumDecl` exists only so that other
    // declarations can name-resolve `<Union>.Tag` as a user-defined
    // type. See `EnumDecl::tag_of_union` for the codegen interplay.
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

/// Maximum bracket/generic nesting depth accepted before parsing.
/// Orders of magnitude above any legitimate AIDL, well below the stack-
/// overflow threshold of the recursive parser/walkers — see
/// [`check_nesting_depth`].
const MAX_NESTING_DEPTH: usize = 256;

/// Pre-parse denial-of-service guard. pest's recursive-descent parser and
/// the recursive AST walkers recurse once per nesting level, so deeply
/// nested input (`((((...))))` or `List<List<...>>`) would overflow the
/// stack and abort the whole process (an uncatchable SIGABRT) on
/// untrusted or build-pipeline-influenced AIDL — `MAX_EXPR_DEPTH` only
/// guards the post-parse expression evaluator, too late to help. This
/// scans the raw source (skipping string/char literals and comments) and
/// returns the byte offset at which `()[]{}` or `<>` nesting first exceeds
/// [`MAX_NESTING_DEPTH`], so the caller can reject it as an ordinary
/// diagnostic. Angle-bracket depth is reset at `;` (a generic type never
/// crosses a statement boundary) so shift/comparison operators in const
/// expressions cannot drift the count into a false positive.
fn check_nesting_depth(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut i = 0;
    let mut bracket_depth: usize = 0; // () [] {}
    let mut angle_depth: usize = 0; // <> generics
    let next = |i: usize| bytes.get(i + 1).copied();
    while i < bytes.len() {
        match bytes[i] {
            // Skip string / char literals (with escapes) so brackets in
            // text are not counted.
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
            b'(' | b'[' | b'{' => bracket_depth += 1,
            b')' | b']' | b'}' => bracket_depth = bracket_depth.saturating_sub(1),
            // `<<` shift / `<=` are not generic openers.
            b'<' if matches!(next(i), Some(b'<') | Some(b'=')) => {
                i += 2;
                continue;
            }
            b'<' => angle_depth += 1,
            // `>>` shift / `>=` are not generic closers.
            b'>' if matches!(next(i), Some(b'>') | Some(b'=')) => {
                i += 2;
                continue;
            }
            b'>' => angle_depth = angle_depth.saturating_sub(1),
            b';' => angle_depth = 0,
            _ => {}
        }
        if bracket_depth > MAX_NESTING_DEPTH || angle_depth > MAX_NESTING_DEPTH {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub fn parse_document(ctx: &SourceContext) -> Result<Document, AidlError> {
    let _guard = SourceGuard::new(&ctx.filename, &ctx.source);
    // DoS guard: reject pathologically nested input *before* handing it to
    // the recursive pest parser, which would otherwise overflow the stack.
    if let Some(offset) = check_nesting_depth(&ctx.source) {
        return Err(ParseError::nesting_too_deep(
            &ctx.filename,
            &ctx.source,
            offset,
            MAX_NESTING_DEPTH,
        )
        .into());
    }
    reset_enum_resolution_state();
    // Take any leftover warnings from a previous call so this document's
    // warning set is scoped to its own parse.
    CURRENT_WARNINGS.with(|w| w.borrow_mut().clear());
    let mut document = Document::new();

    match AIDLParser::parse(Rule::document, &ctx.source) {
        Ok(pairs) => {
            for pair in pairs {
                match pair.as_rule() {
                    Rule::package => {
                        document.package = Some(pair.into_inner().next().unwrap().as_str().into());
                    }

                    Rule::imports => {
                        for pair in pair.into_inner() {
                            let import = pair.as_str().to_string();
                            let key = match import.rfind('.') {
                                Some(idx) => &import[(idx + 1)..],
                                None => &import,
                            };
                            // Two imports with the same simple name but
                            // different fully-qualified names: the map is
                            // keyed by simple name, so the later wins
                            // silently and an unqualified reference would
                            // resolve to the wrong type. AOSP errors on the
                            // conflict; warn here (idempotent re-imports of
                            // the same FQN stay silent).
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
                        document.decls.append(&mut parse_decl(pair.into_inner())?);
                    }

                    Rule::EOI => {}

                    _ => {
                        unreachable!("Unexpected rule in parse_document(): {}", pair)
                    }
                }
            }

            // println!("{:?}", document);
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

    // Drain any non-fatal diagnostics accumulated during this parse so
    // they travel with the document (and don't leak into the next one).
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
    reset_enum_resolution_state();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn test_parse_string_expr() -> Result<(), Box<dyn Error>> {
        let mut res =
            AIDLParser::parse(Rule::string_expr, r##""Hello" + " World""##).map_err(|err| {
                println!("{err}");
                err
            })?;

        let expr = parse_string_expr(res.next().unwrap().into_inner())?;
        assert_eq!(
            expr,
            ConstExpr::new_expr(
                ConstExpr::new(ValueType::String("Hello".into())),
                "+",
                ConstExpr::new(ValueType::String(" World".into()))
            )
        );

        Ok(())
    }

    #[test]
    fn test_parse_expression() -> Result<(), Box<dyn Error>> {
        let mut res =
            AIDLParser::parse(Rule::expression, r##"1 + -3 * 2 << 2 | 4"##).map_err(|err| {
                println!("{err}");
                err
            })?;

        let expr = parse_expression(res.next().unwrap().into_inner())?;
        // assert_eq!(
        //     expr.clone(),
        //     // Expression::Expr {
        //     //     as_str: "1 + -3 * 2 << 2 | 4".into(),
        //     //     lhs: Box::new(Expression::Expr {
        //     //         as_str: "1 + -3 * 2 << 2 | 4".into(),
        //     //         lhs: Box::new(Expression::Expr {
        //     //             as_str: "1 + -3 * 2 << 2 | 4".into(),
        //     //             lhs: Box::new(Expression::Int8(1)),
        //     //             operator: "+".to_string(),
        //     //             rhs: Box::new(Expression::Expr {
        //     //                 as_str: "1 + -3 * 2 << 2 | 4".into(),
        //     //                 lhs: Box::new(Expression::Unary {
        //     //                     operator: "-".to_string(),
        //     //                     expr: Box::new(Expression::Int8(3))
        //     //                 }),
        //     //                 operator: "*".to_string(),
        //     //                 rhs: Box::new(Expression::Int8(2))
        //     //             })
        //     //         }),
        //     //         operator: "<<".to_string(),
        //     //         rhs: Box::new(Expression::Int8(2))
        //     //     }),
        //     //     operator: "|".to_string(),
        //     //     rhs: Box::new(Expression::Int8(4))
        //     // },
        //     ConstExpr::default(),
        // );

        assert_eq!(
            expr.calculate().unwrap(),
            ConstExpr::new(ValueType::Int64(-20))
        );

        Ok(())
    }

    #[test]
    fn test_nesting_depth_guard_rejects_deep_input() {
        // Deeply nested parens / generics would overflow the recursive
        // parser; the pre-scan must flag them (returns the offending
        // offset) before they reach pest.
        let deep_parens = format!("{}1{}", "(".repeat(1000), ")".repeat(1000));
        assert!(check_nesting_depth(&deep_parens).is_some());
        let deep_generics = format!("{}int{}", "List<".repeat(1000), ">".repeat(1000));
        assert!(check_nesting_depth(&deep_generics).is_some());
        // A normal document (and shift/comparison operators) must not trip.
        assert!(check_nesting_depth("interface IFoo { void m(); }").is_none());
        assert!(check_nesting_depth("const int X = 1 << 8 >> 2; const int Y = 3;").is_none());
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
        // `5f`, `10f`, `1e10`, `1E5` must parse as floats — the PEG cannot
        // backtrack leading digits, so each shape is spelled out explicitly.
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
        // `!5` is logical negation (false), not bitwise complement (-6);
        // `!0` is true.
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

    // 1.1n: thread-local state is cleared after SourceGuard is dropped
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

    // 1.1o: thread-local state is cleared even when a panic occurs inside SourceGuard
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
