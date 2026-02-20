// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// #![allow(clippy::missing_const_for_fn)]

use std::cell::RefCell;
use std::collections::HashMap;

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
pub struct Symbol {
    #[allow(dead_code)]
    pub name: String,
    pub value: crate::const_expr::ConstExpr,
    #[allow(dead_code)]
    pub symbol_type: SymbolType,
    #[allow(dead_code)]
    pub namespace: Option<String>,
}

thread_local! {
    static DECLARATION_MAP: RefCell<HashMap<Namespace, Declaration>> = RefCell::new(HashMap::new());
    #[allow(clippy::missing_const_for_thread_local)]
    static NAMESPACE_STACK: RefCell<Vec<Namespace>> = RefCell::new(Vec::new());
    static DOCUMENT: RefCell<Document> = RefCell::new(Document::new());

    // Universal Symbol Table - supports all types of named constants
    static SYMBOL_TABLE: RefCell<HashMap<String, Symbol>> = RefCell::new(HashMap::new());

    // Filename and source text of the source currently being parsed (used for error message generation)
    #[allow(clippy::missing_const_for_thread_local)]
    static CURRENT_SOURCE_NAME: RefCell<String> = RefCell::new(String::new());
    #[allow(clippy::missing_const_for_thread_local)]
    static CURRENT_SOURCE_TEXT: RefCell<String> = RefCell::new(String::new());
}

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
    NAMESPACE_STACK.with(|stack| {
        stack.borrow().last().cloned().unwrap_or_default()
    })
}

pub fn set_current_document(document: &Document) {
    DOCUMENT.with(|doc| {
        let mut doc = doc.borrow_mut();

        doc.package = document.package.clone();
        doc.imports = document.imports.clone();
    })
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

    // leave max 2 items because the other items are for name space.
    if namespace.ns.len() > 2 {
        namespace.ns.drain(0..namespace.ns.len() - 2);
    }

    Some(LookupDecl {
        decl,
        ns,
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
                    return Some(make_const_expr(None, lookup_decl));
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

// Universal symbol registration - supports all types of named constants
pub fn register_symbol(
    name: &str,
    value: ConstExpr,
    symbol_type: SymbolType,
    namespace: Option<&str>,
) {
    let symbol = Symbol {
        name: name.to_string(),
        value,
        symbol_type,
        namespace: namespace.map(|s| s.to_string()),
    };

    SYMBOL_TABLE.with(|table| {
        let mut table = table.borrow_mut();

        // Register with simple name
        table.insert(name.to_string(), symbol.clone());

        // Also register with qualified name if namespace is provided
        if let Some(ns) = namespace {
            let qualified_name = format!("{}.{}", ns, name);
            table.insert(qualified_name, symbol);
        }
    });
}

// Note: register_enum_member removed as it's not used
// Use register_symbol directly with SymbolType::EnumMember

// Enhanced name resolution with universal symbol table
pub fn name_to_const_expr(name: &str) -> Option<ConstExpr> {
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
}

impl Document {
    fn new() -> Self {
        Self {
            package: None,
            imports: HashMap::new(),
            decls: Vec::new(),
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
            decl.const_expr = decl.const_expr.as_ref().map(|expr| expr.calculate().unwrap_or_else(|_| expr.clone()));
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
    pub members: Vec<Declaration>,
    // pub name_dict: Option<HashMap<String, ConstExpr>>,
}

impl ParcelableDecl {
    pub fn pre_process(&mut self) {
        for decl in &mut self.members {
            if let Declaration::Variable(decl) = decl {
                decl.const_expr = decl.const_expr.as_ref().map(|expr| expr.calculate().unwrap_or_else(|_| expr.clone()));
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

    // fn arg_identifier(&self) -> String {
    //     format!("_arg_{}", self.identifier)
    // }

    // pub fn to_string(&self, is_nullable: bool) -> (String, String, String, String) {
    //     let param = self.arg_identifier();
    //     let mut type_cast = self.r#type.type_cast();
    //     let type_cloned = type_cast.clone();
    //     type_cast.set_fn_nullable(is_nullable);
    //     let def_arg = type_cast.fn_def_arg(&self.direction);
    //     let arg = format!("{}: {}",
    //         param.clone(), def_arg);
    //     (param, arg, def_arg, type_cloned.return_type())
    // }

    pub fn is_mutable(&self) -> bool {
        match self.direction {
            Direction::Inout | Direction::Out => true,
            Direction::In => false,
            _ => false,
        }
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

// fn generic_type_args_to_string(args: &[Type]) -> String {
//     let mut args_str = String::new();

//     args.iter().for_each(|t| {
//         let mut cast = t.type_cast();
//         cast.set_generic(true);

//         args_str.push_str(", ");
//         args_str.push_str(&cast.member_type());
//     });

//     args_str[2..].into()
// }

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
    RustDerive,
    VintfStability,
}

pub fn check_annotation_list(
    annotation_list: &Vec<Annotation>,
    query_type: AnnotationType,
) -> (bool, String) {
    for annotation in annotation_list {
        match query_type {
            AnnotationType::VintfStability if annotation.annotation == "@VintfStability" => {
                return (true, "".to_owned())
            }
            AnnotationType::IsNullable if annotation.annotation == "@nullable" => {
                return (true, "".to_owned())
            }
            AnnotationType::JavaOnly if annotation.annotation.starts_with("@JavaOnly") => {
                return (true, "".to_owned())
            }
            AnnotationType::RustDerive if annotation.annotation == "@RustDerive" => {
                let mut derives = Vec::new();

                for param in &annotation.parameter_list {
                    if param.const_expr.to_bool().unwrap_or(false) {
                        derives.push(param.identifier.to_owned())
                    }
                }

                return (true, derives.join(","));
            }
            _ => {}
        }
    }

    (false, "".to_owned())
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
                    return type_generator::TypeGenerator::new(&NonArrayType {
                        // The cstr is enclosed in quotes.
                        name: param.const_expr.to_value_string().trim_matches('"').into(),
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

    let value = if value.ends_with('l') || value.ends_with('L') {
        is_long = true;
        &value[..value.len() - 1]
    } else if let Some(stripped) = value.strip_suffix("u8") {
        is_u8 = true;
        stripped
    } else {
        value
    };

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
        Rule::expression => parse_expression(pair.clone().into_inner()),
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
        | Rule::logical_and => parse_expression(pair.clone().into_inner()),
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

fn parse_string_term(pair: pest::iterators::Pair<Rule>) -> ConstExpr {
    match pair.as_rule() {
        Rule::C_STR => {
            let string = pair.as_str();
            ConstExpr::new(ValueType::String(string[1..string.len() - 1].into()))
        }
        Rule::qualified_name => ConstExpr::new(ValueType::Name(pair.as_str().into())),
        _ => unreachable!("Unexpected rule in Rule::parse_string_term: {}", pair),
    }
}

fn parse_string_expr(pairs: pest::iterators::Pairs<Rule>) -> Result<ConstExpr, AidlError> {
    let mut expr: Option<ConstExpr> = None;

    for pair in pairs {
        match pair.as_rule() {
            Rule::string_term => {
                let term = parse_string_term(pair.into_inner().next().unwrap());
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
                        value_list.push(parse_const_expr(pair.into_inner().next().unwrap())?);
                    }
                    _ => unreachable!("Unexpected rule in Rule::constant_value_list: {}", pair),
                }
            }
            Ok(ConstExpr::new(ValueType::Array(value_list)))
        }

        Rule::CHARVALUE => {
            let mut found = false;
            let mut has_backslash = false;
            for ch in pair.as_str().chars() {
                if !found && ch == '\'' {
                    found = true;
                } else if found {
                    if !has_backslash && ch == '\\' {
                        has_backslash = true;
                    } else {
                        return Ok(ConstExpr::new(ValueType::Char(ch)));
                    }
                }
            }
            unreachable!()
        }

        Rule::expression => parse_expression(pair.clone().into_inner()),

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
                parameter.const_expr = parse_const_expr(pair.into_inner().next().unwrap())?;
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
    let mut annotation = Annotation::default();
    for pair in pairs {
        match pair.as_rule() {
            Rule::ANNOTATION => {
                let span = pair.as_span();
                annotation.annotation = pair.as_str().into();
                annotation.annotation_span = Some((span.start(), span.end()));
            }

            Rule::const_expr => {
                annotation.const_expr = Some(parse_const_expr(pair.into_inner().next().unwrap())?);
            }

            Rule::parameter_list => {
                annotation.parameter_list = parse_parameter_list(pair.into_inner())?;
            }

            _ => unreachable!("Unexpected rule in parse_annotation(): {}", pair),
        }
    }

    Ok(annotation)
}

fn parse_annotation_list(pairs: pest::iterators::Pairs<Rule>) -> Result<Vec<Annotation>, AidlError> {
    let mut annotation_list = Vec::new();
    for pair in pairs {
        annotation_list.push(parse_annotation(pair.into_inner())?);
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
                array_type.const_expr = Some(parse_const_expr(pair.into_inner().next().unwrap())?);
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
                r#type.array_types.push(parse_array_type(pair.into_inner())?);
            }
            _ => {
                unreachable!("Unexpected rule in parse_type(): {}", pair);
            }
        }
    }

    Ok(r#type)
}

fn parse_variable_decl(pairs: pest::iterators::Pairs<Rule>, constant: bool) -> Result<VariableDecl, AidlError> {
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
                let expr = parse_intvalue(pair.as_str(), (span.start(), span.end()))?.calculate()
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

fn parse_interface_members(pairs: pest::iterators::Pairs<Rule>, interface: &mut InterfaceDecl) -> Result<(), AidlError> {
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
                interface.members.append(&mut parse_decl(pair.into_inner())?);
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

    Ok(Declaration::Interface(interface))
}

fn parse_parcelable_members(pairs: pest::iterators::Pairs<Rule>) -> Result<Vec<Declaration>, AidlError> {
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

            Rule::C_STR => {
                parcelable.cpp_header = pair.as_str().into();
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
}

fn parse_enumerator(pairs: pest::iterators::Pairs<Rule>) -> Result<Enumerator, AidlError> {
    let mut res = Enumerator::default();

    for pair in pairs {
        match pair.as_rule() {
            Rule::identifier => {
                res.identifier = pair.as_str().into();
            }
            Rule::const_expr => {
                res.const_expr = Some(parse_const_expr(pair.into_inner().next().unwrap())?);
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
                declarations.push(parse_union_decl(annotation_list.clone(), pair.into_inner())?);
            }

            _ => unreachable!("Unexpected rule in parse_decl(): {}", pair),
        };
    }

    Ok(declarations)
}

pub fn calculate_namespace(decl: &mut Declaration, mut namespace: Namespace) {
    if decl.is_variable().is_some() {
        return;
    }

    namespace.push(decl.name());

    decl.set_namespace(namespace.clone());

    DECLARATION_MAP.with(|hashmap| {
        hashmap.borrow_mut().insert(namespace.clone(), decl.clone());
    });

    for decl in decl.members_mut() {
        calculate_namespace(decl, namespace.clone());
    }
}

pub fn parse_document(ctx: &SourceContext) -> Result<Document, AidlError> {
    let _guard = SourceGuard::new(&ctx.filename, &ctx.source);
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
            return Err(
                pest_error_to_diagnostic(err, &ctx.filename, &ctx.source).into(),
            );
        }
    }

    let namespace = if let Some(ref package) = document.package {
        Namespace::new(package, Namespace::AIDL)
    } else {
        Namespace::default()
    };

    for decl in &mut document.decls {
        calculate_namespace(decl, namespace.clone());
    }

    Ok(document)
}

pub fn reset() {
    DECLARATION_MAP.with(|hashmap| {
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

        assert_eq!(expr.calculate().unwrap(), ConstExpr::new(ValueType::Int64(-20)));

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
