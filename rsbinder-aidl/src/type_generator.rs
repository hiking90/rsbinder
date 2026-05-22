// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::OnceLock;

use miette::{NamedSource, SourceSpan};

use crate::const_expr::{ConstExpr, InitParam, ValueType};
use crate::error::{AidlError, ResolutionError, SemanticError};
use crate::parser::{self, *};

/// Resolves the current source name/span into the `(NamedSource, SourceSpan)`
/// pair every type-level diagnostic needs, falling back to a placeholder
/// filename when no source context is active (e.g. unit tests).
fn diagnostic_source(span: Option<(usize, usize)>) -> (NamedSource<String>, SourceSpan) {
    let filename = parser::current_source_name();
    let source = parser::current_source_text();
    let (start, end) = span.unwrap_or((0, 0));
    let src_name = if filename.is_empty() {
        "<type_generator>".to_string()
    } else {
        filename
    };
    (
        NamedSource::new(src_name, source),
        SourceSpan::new(start.into(), end.saturating_sub(start)),
    )
}

fn make_type_error(message: impl Into<String>, span: Option<(usize, usize)>) -> AidlError {
    let (src, span) = diagnostic_source(span);
    AidlError::from(SemanticError::InvalidOperation {
        message: message.into(),
        src,
        span,
    })
}

static CRATE_NAME: OnceLock<String> = OnceLock::new();

pub fn crate_name() -> &'static str {
    CRATE_NAME.get_or_init(|| "rsbinder".to_owned()).as_str()
}

pub fn set_crate_support(support: bool) {
    CRATE_NAME.get_or_init(|| {
        if support {
            "crate".to_owned()
        } else {
            "rsbinder".to_owned()
        }
    });
}

#[derive(Clone, Debug)]
struct ArrayInfo {
    sizes: Vec<i64>,
    value_type: ValueType,
    is_list: bool,
}

impl ArrayInfo {
    fn new(value_type: &ValueType, array_types: &[parser::ArrayType]) -> Self {
        Self {
            sizes: array_types
                .iter()
                .map(|t| {
                    t.const_expr.clone().map_or_else(
                        || 0,
                        |v| v.calculate().and_then(|c| c.to_i64()).unwrap_or(0),
                    )
                })
                .collect(),
            value_type: value_type.clone(),
            is_list: false,
        }
    }

    fn new_list(value_type: &ValueType, array_types: &[parser::ArrayType]) -> Self {
        let mut this = Self::new(value_type, array_types);
        this.is_list = true;
        this
    }

    fn is_fixed(&self) -> bool {
        !self.sizes.is_empty() && { self.sizes.iter().all(|size| *size > 0) }
    }
}

#[derive(Clone)]
pub struct TypeGenerator {
    pub(crate) is_nullable: bool,
    pub value_type: ValueType,
    array_types: Vec<ArrayInfo>,
    pub identifier: String,
    direction: Direction,
    type_span: Option<(usize, usize)>,
}

impl TypeGenerator {
    pub fn new(aidl_type: &NonArrayType) -> Result<Self, AidlError> {
        let mut array_types = Vec::new();
        let value_type = match aidl_type.name.as_str() {
            "boolean" => ValueType::Bool(false),
            "byte" => ValueType::Byte(0),
            "char" => ValueType::Char(Default::default()),
            "int" => ValueType::Int32(0),
            "long" => ValueType::Int64(0),
            "float" => ValueType::Float(0.),
            "double" => ValueType::Double(0.),
            "void" => ValueType::Void,
            "String" => ValueType::String(String::new()),
            "IBinder" => ValueType::IBinder,
            "List" => match &aidl_type.generic {
                Some(gen) => {
                    array_types.push(ArrayInfo::new_list(&gen.to_value_type()?, &Vec::new()));
                    ValueType::Array(Vec::new())
                }
                None => {
                    return Err(make_type_error(
                        "Type \"List\" of AIDL must have Generic Type",
                        aidl_type.name_span,
                    ))
                }
            },
            "FileDescriptor" => {
                let (src, span) = diagnostic_source(aidl_type.name_span);
                return Err(AidlError::from(SemanticError::UnsupportedType {
                    type_name: "FileDescriptor".to_string(),
                    help: Some("Use ParcelFileDescriptor instead".to_string()),
                    src,
                    span,
                }));
            }
            "ParcelFileDescriptor" => ValueType::FileDescriptor,
            "ParcelableHolder" => ValueType::Holder,
            _ => ValueType::UserDefined(aidl_type.name.to_owned()),
        };

        Ok(Self {
            is_nullable: false,
            value_type,
            array_types,
            identifier: String::new(),
            direction: Default::default(),
            type_span: aidl_type.name_span,
        })
    }

    pub fn new_with_type(_type: &Type) -> Result<Self, AidlError> {
        let mut this = Self::new(&_type.non_array_type)?;

        if !_type.array_types.is_empty() {
            this = this.array(&_type.array_types);
        }

        if has_annotation(&_type.annotation_list, AnnotationType::IsNullable) {
            let nullable_span = _type
                .annotation_list
                .iter()
                .find(|a| a.annotation == "@nullable")
                .and_then(|a| a.annotation_span);
            this.nullable_at(nullable_span)
        } else {
            Ok(this)
        }
    }

    /// Verify that every user-defined type this generator references resolves
    /// to a known declaration in the current namespace context.
    ///
    /// Downstream string builders (`make_user_defined_type_name`) assume
    /// resolution always succeeds and `expect()` otherwise; calling this at each
    /// type-construction site turns an undefined type into a proper
    /// `ResolutionError::UnknownType` diagnostic instead of a panic. Must be
    /// invoked while the owning declaration's `NamespaceGuard` is active.
    pub fn ensure_resolvable(&self) -> Result<(), AidlError> {
        let check = |value_type: &ValueType| -> Result<(), AidlError> {
            if let ValueType::UserDefined(name) = value_type {
                // `lookup_decl_from_name` falls back to the *current* namespace's
                // own declaration when nothing matches, so an undefined type does
                // not return `None`; confirm the resolved declaration's simple
                // name actually equals the requested one to detect that case.
                let requested = name.rsplit('.').next().unwrap_or(name.as_str());
                let resolved = lookup_decl_from_name(name, crate::Namespace::AIDL)
                    .filter(|lookup_decl| lookup_decl.decl.name() == requested);
                if resolved.is_none() {
                    let (src, span) = diagnostic_source(self.type_span);
                    return Err(AidlError::from(ResolutionError::UnknownType {
                        name: name.clone(),
                        src,
                        span,
                    }));
                }
            }
            Ok(())
        };

        check(&self.value_type)?;
        for array_info in &self.array_types {
            check(&array_info.value_type)?;
        }
        Ok(())
    }

    fn is_aidl_nullable(value_type: &ValueType) -> bool {
        match value_type {
            ValueType::String(_)
            | ValueType::Array(_)
            | ValueType::FileDescriptor
            | ValueType::IBinder => true,
            ValueType::UserDefined(name) => {
                match lookup_decl_from_name(name, crate::Namespace::AIDL) {
                    Some(lookup_decl) => !matches!(lookup_decl.decl, Declaration::Enum(_)),
                    None => true, // Unknown types are treated as nullable (non-primitive)
                }
            }
            _ => false,
        }
    }

    fn make_user_defined_type_name(&self, type_name: &str) -> String {
        let lookup_decl = lookup_decl_from_name(type_name, crate::Namespace::AIDL)
            .expect("type must be resolved during code generation");
        let curr_ns = current_namespace();
        let ns = curr_ns.relative_mod(&lookup_decl.ns);
        let name = if !ns.is_empty() {
            format!("{}::{}", ns, lookup_decl.name.ns.last().unwrap())
        } else {
            let name: String = lookup_decl.name.ns.last().unwrap().to_owned();
            if curr_ns.ns.last().unwrap() == name.as_str() {
                format!("Box<{name}>") // To avoid, recursive type issue.
            } else {
                name
            }
        };

        match lookup_decl.decl {
            Declaration::Interface(_) => {
                format!("{}::Strong<dyn {}>", crate_name(), name)
            }
            _ => name,
        }
    }

    // AIDL Enum is a kind of primitive type.
    fn is_primitive(value_type: &ValueType) -> bool {
        match value_type {
            ValueType::UserDefined(name) => {
                match lookup_decl_from_name(name, crate::Namespace::AIDL) {
                    Some(lookup_decl) => matches!(lookup_decl.decl, Declaration::Enum(_)),
                    None => false, // Unknown types are not primitive
                }
            }
            ValueType::Reference { .. } => true,
            _ => value_type.is_primitive(),
        }
    }

    fn is_interface(value_type: &ValueType) -> bool {
        match value_type {
            ValueType::UserDefined(name) => {
                matches!(
                    lookup_decl_from_name(name, crate::Namespace::AIDL),
                    Some(lookup_decl) if matches!(lookup_decl.decl, Declaration::Interface(_))
                )
            }
            _ => false,
        }
    }

    pub fn is_variable_array(&self) -> bool {
        if matches!(self.value_type, ValueType::Array(_)) {
            let sub_type = self.array_types.first().expect("array_types is empty.");
            if !sub_type.is_fixed() && !sub_type.is_list {
                return true;
            }
        }
        false
    }

    // Check if this type can be initialized with Default::default().
    pub fn can_be_defaulted(value_type: &ValueType, is_struct: bool) -> bool {
        if is_struct {
            Self::is_primitive(value_type)
                || match value_type {
                    ValueType::String(_)
                    | ValueType::Array(_)
                    | ValueType::Map(_, _)
                    | ValueType::Holder => true,
                    ValueType::UserDefined(name) => {
                        match lookup_decl_from_name(name, crate::Namespace::AIDL) {
                            // Strong<dyn IFoo> has no sensible Default, so struct
                            // interface fields must be represented as Option<Strong<_>>.
                            Some(lookup_decl) => {
                                !matches!(lookup_decl.decl, Declaration::Interface(_))
                            }
                            None => true,
                        }
                    }
                    _ => false,
                }
        } else {
            Self::is_primitive(value_type)
                || match value_type {
                    ValueType::String(_)
                    | ValueType::Array(_)
                    | ValueType::Map(_, _)
                    | ValueType::Holder => true,
                    ValueType::UserDefined(name) => {
                        match lookup_decl_from_name(name, crate::Namespace::AIDL) {
                            Some(lookup_decl) => matches!(
                                lookup_decl.decl,
                                Declaration::Enum(_)
                                    | Declaration::Parcelable(_)
                                    | Declaration::Union(_)
                            ),
                            None => false,
                        }
                    }
                    _ => false,
                }
        }
    }

    pub fn nullable_at(
        mut self,
        annotation_span: Option<(usize, usize)>,
    ) -> Result<Self, AidlError> {
        if Self::is_primitive(&self.value_type) {
            return Err(make_type_error(
                format!(
                    "Primitive type({:?}) cannot get nullable annotation",
                    self.value_type
                ),
                annotation_span,
            ));
        }
        self.is_nullable = true;
        Ok(self)
    }

    pub fn nullable(self) -> Result<Self, AidlError> {
        self.nullable_at(None)
    }

    pub fn identifier(mut self, ident: &str) -> Self {
        self.identifier = format!("_arg_{ident}");
        self
    }

    pub fn direction_at(
        mut self,
        direction: &Direction,
        direction_span: Option<(usize, usize)>,
    ) -> Result<Self, AidlError> {
        if matches!(direction, Direction::Out | Direction::Inout)
            && (Self::is_primitive(&self.value_type)
                || matches!(self.value_type, ValueType::String(_)))
        {
            return Err(make_type_error(
                "Primitive types and String cannot be an out or inout parameter",
                direction_span.or(self.type_span),
            ));
        }
        self.direction = direction.clone();
        Ok(self)
    }

    pub fn direction(self, direction: &Direction) -> Result<Self, AidlError> {
        self.direction_at(direction, None)
    }

    // Switch to array type.
    pub fn array(mut self, array_types: &[parser::ArrayType]) -> Self {
        match self.value_type {
            ValueType::Array(_) => self,
            _ => {
                self.array_types
                    .push(ArrayInfo::new(&self.value_type, array_types));
                self.value_type = ValueType::Array(Vec::new());
                self
            }
        }
    }

    fn array_type_name(&self, value_type: &ValueType) -> String {
        let name = self.type_decl(value_type);
        if name == "i8" {
            "u8".to_owned()
        } else {
            name
        }
    }

    fn make_fixed_array(&self, array_info: &ArrayInfo, is_struct: bool) -> String {
        assert!(!array_info.sizes.is_empty());

        let type_name = self.array_type_name(&array_info.value_type);

        let value_str = if is_struct {
            if (self.is_nullable && Self::is_aidl_nullable(&array_info.value_type))
                || !Self::can_be_defaulted(&array_info.value_type, is_struct)
            {
                format!("Option<{type_name}>")
            } else {
                type_name
            }
        } else {
            match self.direction {
                Direction::Out | Direction::Inout => {
                    if self.is_nullable
                        || !Self::can_be_defaulted(&array_info.value_type, is_struct)
                    {
                        format!("Option<{type_name}>")
                    } else {
                        type_name
                    }
                }
                _ => type_name,
            }
        };

        array_info.sizes.iter().rev().skip(1).fold(
            format!("[{}; {}]", value_str, array_info.sizes.last().unwrap()),
            |acc, size| format!("[{acc}; {size}]"),
        )
    }

    fn list_type_decl_fixed(&self, array_info: &ArrayInfo, is_struct: bool) -> String {
        // A fixed-size array's wrapping is direction-independent: only
        // nullability decides whether it is wrapped in `Option<_>`.
        let fixed_array = self.make_fixed_array(array_info, is_struct);
        if self.is_nullable {
            format!("Option<{fixed_array}>")
        } else {
            fixed_array
        }
    }

    fn list_type_decl(&self, is_struct: bool) -> String {
        let sub_type = self.array_types.first().expect("array_types is empty.");
        if sub_type.is_fixed() {
            return self.list_type_decl_fixed(sub_type, is_struct);
        }

        let type_name = self.array_type_name(&sub_type.value_type);
        match self.direction {
            Direction::Out => {
                if self.is_nullable {
                    format!("Vec<Option<{type_name}>>")
                } else if Self::can_be_defaulted(&sub_type.value_type, is_struct) {
                    format!("Vec<{type_name}>")
                } else {
                    format!("Vec<Option<{type_name}>>")
                }
            }
            Direction::Inout => {
                if self.is_nullable {
                    format!("Vec<Option<{type_name}>>")
                } else if Self::can_be_defaulted(&sub_type.value_type, true)
                    || Self::is_interface(&sub_type.value_type)
                {
                    format!("Vec<{type_name}>")
                } else {
                    format!("Vec<Option<{type_name}>>")
                }
            }
            _ => {
                if is_struct {
                    if self.is_nullable && Self::is_aidl_nullable(&sub_type.value_type) {
                        format!("Vec<Option<{type_name}>>")
                    } else {
                        format!("Vec<{type_name}>")
                    }
                } else if self.is_nullable {
                    if Self::is_primitive(&sub_type.value_type) {
                        format!("Option<Vec<{type_name}>>")
                    } else {
                        format!("Option<Vec<Option<{type_name}>>>")
                    }
                } else {
                    format!("Vec<{type_name}>")
                }
            }
        }
    }

    fn type_decl(&self, value_type: &ValueType) -> String {
        match value_type {
            ValueType::Void => "()".into(),
            ValueType::String(_) => "String".into(),
            ValueType::Byte(_) => "i8".into(),
            ValueType::Int32(_) => "i32".into(),
            ValueType::Int64(_) => "i64".into(),
            ValueType::Float(_) => "f32".into(),
            ValueType::Double(_) => "f64".into(),
            ValueType::Bool(_) => "bool".into(),
            ValueType::Char(_) => "u16".into(),
            ValueType::Array(_) => {
                // Vec<> is managed other functions. Therefore, here we just use a panic.
                panic!("type_decl() can't process Array Type.")
                // Self::type_decl(sub_value.expect("Array must know the type of item."), None)
            }
            ValueType::IBinder => format!("{}::SIBinder", crate_name()),
            ValueType::FileDescriptor => format!("{}::ParcelFileDescriptor", crate_name()),
            ValueType::Holder => format!("{}::ParcelableHolder", crate_name()),
            ValueType::UserDefined(name) => self.make_user_defined_type_name(name),
            _ => unreachable!(),
        }
    }

    pub fn type_declaration(&self, is_struct: bool) -> String {
        let mut is_nullable = self.is_nullable;
        let name = match &self.value_type {
            ValueType::Array(_) => self.list_type_decl(is_struct),
            _ => {
                if !Self::can_be_defaulted(&self.value_type, is_struct) && is_struct {
                    is_nullable = true;
                }
                self.type_decl(&self.value_type)
            }
        };

        if is_nullable && !name.starts_with("Option<") {
            format!("Option<{name}>")
        } else {
            name
        }
    }

    fn func_list_type_decl_fixed(&self, array_info: &ArrayInfo) -> String {
        let fixed_array = self.make_fixed_array(array_info, false);

        match self.direction {
            Direction::Out | Direction::Inout => {
                if self.is_nullable {
                    format!("&mut Option<{fixed_array}>")
                } else {
                    format!("&mut {fixed_array}")
                }
            }
            _ => {
                if self.is_nullable {
                    format!("Option<&{fixed_array}>")
                } else {
                    format!("&{fixed_array}")
                }
            }
        }
    }

    fn func_list_type_decl(&self) -> String {
        let sub_type = self.array_types.first().expect("array_types is empty.");
        if sub_type.is_fixed() {
            return self.func_list_type_decl_fixed(sub_type);
        }
        let type_name = self.array_type_name(&sub_type.value_type);
        match self.direction {
            Direction::Out => {
                if self.is_nullable {
                    // if nullable, it means that the array can have null elements.
                    format!("&mut Option<Vec<Option<{type_name}>>>")
                } else if Self::can_be_defaulted(&sub_type.value_type, false)
                    || Self::is_primitive(&sub_type.value_type)
                {
                    // Enum is a primitive type.
                    format!("&mut Vec<{type_name}>")
                } else {
                    format!("&mut Vec<Option<{type_name}>>")
                }
            }
            Direction::Inout => {
                if self.is_nullable {
                    if Self::can_be_defaulted(&sub_type.value_type, false) {
                        format!("&mut Option<Vec<{type_name}>>")
                    } else {
                        format!("&mut Option<Vec<Option<{type_name}>>>")
                    }
                } else {
                    format!("&mut Vec<{type_name}>")
                }
            }
            _ => {
                if self.is_nullable {
                    if Self::is_primitive(&sub_type.value_type) {
                        format!("Option<&[{type_name}]>")
                    } else {
                        format!("Option<&[Option<{type_name}>]>")
                    }
                } else {
                    format!("&[{type_name}]")
                }
            }
        }
    }

    pub fn type_decl_for_func(&self) -> Result<String, AidlError> {
        Ok(match &self.value_type {
            ValueType::Array(_) => self.func_list_type_decl(),
            ValueType::String(_) => match self.direction {
                Direction::Out | Direction::Inout => {
                    return Err(make_type_error(
                        "String cannot be an out or inout parameter",
                        self.type_span,
                    ))
                }
                _ => {
                    if self.is_nullable {
                        "Option<&str>".into()
                    } else {
                        "&str".into()
                    }
                }
            },
            _ => match self.direction {
                Direction::Out | Direction::Inout => {
                    if Self::is_primitive(&self.value_type) {
                        return Err(make_type_error(
                            format!("{:?} cannot be an out or inout parameter", self.value_type),
                            self.type_span,
                        ));
                    }
                    let name = self.type_decl(&self.value_type);
                    if self.is_nullable {
                        format!("&mut Option<{name}>")
                    } else {
                        format!("&mut {name}")
                    }
                }
                _ => {
                    if Self::is_primitive(&self.value_type) {
                        self.type_decl(&self.value_type)
                    } else {
                        let name = self.type_decl(&self.value_type);
                        if self.is_nullable {
                            format!("Option<&{name}>")
                        } else {
                            format!("&{name}")
                        }
                    }
                }
            },
        })
    }

    pub fn const_type_decl(&self) -> Result<String, AidlError> {
        self.clone().direction(&Direction::In)?.type_decl_for_func()
    }

    fn check_identifier(&self) {
        assert!(!self.identifier.is_empty(), "identifier is empty");
    }

    pub fn func_call_param(&self) -> String {
        self.check_identifier();

        if Self::is_primitive(&self.value_type) {
            self.identifier.clone()
        } else {
            let decl = self.type_declaration(false);

            if decl == "String" {
                format!("{}.as_str()", self.identifier)
            } else {
                match self.direction {
                    Direction::Inout | Direction::Out => {
                        format!("&mut {}", self.identifier)
                    }
                    _ => {
                        if decl.starts_with("Option<Vec<") || decl.starts_with("Option<String>") {
                            format!("{}.as_deref()", self.identifier)
                        } else if decl.starts_with("Option<") {
                            format!("{}.as_ref()", self.identifier)
                        } else {
                            format!("&{}", self.identifier)
                        }
                    }
                }
            }
        }
    }

    pub fn transaction_decl(&self, reader: &str) -> String {
        self.check_identifier();

        let (mutable, init) = match self.direction {
            Direction::Out => ("mut ", "Default::default()".to_owned()),
            Direction::Inout => ("mut ", format!("{reader}.read()?")),
            _ => ("", format!("{reader}.read()?")),
        };

        format!(
            "{mutable}{}: {} = {init}",
            self.identifier,
            self.type_declaration(false)
        )
    }

    pub fn default_value(&self) -> String {
        match &self.value_type {
            ValueType::UserDefined(name) => {
                match lookup_decl_from_name(name, crate::Namespace::AIDL) {
                    Some(lookup_decl) => match lookup_decl.decl {
                        Declaration::Enum(enum_decl) => {
                            let first = enum_decl.enumerator_list.first().unwrap();
                            format!(
                                "{}::{}",
                                self.make_user_defined_type_name(name),
                                first.identifier
                            )
                        }
                        _ => "Default::default()".to_owned(),
                    },
                    None => "Default::default()".to_owned(),
                }
            }
            _ => "Default::default()".to_owned(),
        }
    }

    fn enum_lookup(&self) -> Option<LookupDecl> {
        let ValueType::UserDefined(name) = &self.value_type else {
            return None;
        };

        let lookup_decl = lookup_decl_from_name(name, crate::Namespace::AIDL)?;
        if matches!(&lookup_decl.decl, Declaration::Enum(_)) {
            Some(lookup_decl)
        } else {
            None
        }
    }

    fn resolve_enum_reference_for_target(expr: &ConstExpr, target_enum: &str) -> ConstExpr {
        match &expr.value {
            ValueType::Name(name) => {
                parser::name_to_enum_member_const_expr(name, Some(target_enum))
                    .unwrap_or_else(|| expr.clone())
            }
            ValueType::Array(values) => ConstExpr::new(ValueType::Array(
                values
                    .iter()
                    .map(|value| Self::resolve_enum_reference_for_target(value, target_enum))
                    .collect(),
            )),
            ValueType::Expr { lhs, operator, rhs } => ConstExpr::new(ValueType::Expr {
                lhs: Box::new(Self::resolve_enum_reference_for_target(lhs, target_enum)),
                operator: operator.clone(),
                rhs: Box::new(Self::resolve_enum_reference_for_target(rhs, target_enum)),
            }),
            ValueType::Unary { operator, expr } => ConstExpr::new(ValueType::Unary {
                operator: operator.clone(),
                expr: Box::new(Self::resolve_enum_reference_for_target(expr, target_enum)),
            }),
            _ => expr.clone(),
        }
    }

    fn validate_enum_value(
        &self,
        expr: &ConstExpr,
        target_lookup: &LookupDecl,
    ) -> Result<ConstExpr, AidlError> {
        let target_enum = target_lookup.ns.to_string(crate::Namespace::AIDL);
        let resolved = Self::resolve_enum_reference_for_target(expr, &target_enum);
        let calculated = resolved
            .calculate()
            .map_err(|e| make_type_error(e.message, self.type_span))?;

        match &calculated.value {
            ValueType::Reference { enum_type, .. } => {
                // Same member names are common across enums; ensure a default
                // belongs to the field's enum family before rendering Rust.
                if enum_type != &target_enum {
                    return Err(make_type_error(
                        format!(
                            "enum default value {} does not match target enum {}",
                            calculated.to_value_string(),
                            target_enum
                        ),
                        self.type_span,
                    ));
                }

                Ok(calculated)
            }
            ValueType::Name(name) => Err(make_type_error(
                format!(
                    "unresolved enum default value {} for target enum {}",
                    name, target_enum
                ),
                self.type_span,
            )),
            _ => Err(make_type_error(
                format!(
                    "enum default value {} is not a member of target enum {}",
                    calculated.to_value_string(),
                    target_enum
                ),
                self.type_span,
            )),
        }
    }

    fn init_enum_value(
        &self,
        expr: &ConstExpr,
        target_lookup: &LookupDecl,
        param: InitParam,
    ) -> Result<String, AidlError> {
        let calculated = self.validate_enum_value(expr, target_lookup)?;
        Ok(calculated
            .value
            .to_init(param.with_fixed_array(false).with_nullable(false)))
    }

    fn init_enum_array_value(
        &self,
        expr: &ConstExpr,
        target_lookup: &LookupDecl,
        param: InitParam,
        is_fixed_array: bool,
        is_nullable: bool,
    ) -> Result<String, AidlError> {
        let target_enum = target_lookup.ns.to_string(crate::Namespace::AIDL);
        let resolved = Self::resolve_enum_reference_for_target(expr, &target_enum);
        let calculated = resolved
            .calculate()
            .map_err(|e| make_type_error(e.message, self.type_span))?;

        let ValueType::Array(values) = calculated.value else {
            return Err(make_type_error(
                format!(
                    "enum array default value {} is not an array",
                    calculated.to_value_string()
                ),
                self.type_span,
            ));
        };

        let mut enum_values = Vec::new();
        for value in values {
            enum_values.push(self.validate_enum_value(&value, target_lookup)?);
        }

        Ok(ValueType::Array(enum_values).to_init(
            param
                .with_fixed_array(is_fixed_array)
                .with_nullable(is_nullable),
        ))
    }

    fn init_array_value(
        expr: &ConstExpr,
        array_info: &ArrayInfo,
        param: InitParam,
        is_nullable: bool,
    ) -> String {
        match expr
            .calculate()
            .and_then(|c| c.convert_to(&array_info.value_type))
        {
            Ok(converted) => converted.value.to_init(
                param
                    .with_fixed_array(array_info.is_fixed())
                    .with_nullable(is_nullable),
            ),
            Err(_) => ValueType::Void.to_init(param.with_fixed_array(false).with_nullable(false)),
        }
    }

    /// Renders the initializer for an array-typed field/const default.
    /// Returns the bare initializer; the outer `Option` wrap (when nullable) is
    /// applied by `init_value`.
    fn init_array_branch(&self, expr: &ConstExpr, param: InitParam) -> Result<String, AidlError> {
        let array_info = self.array_types.first().expect("array_types is empty.");
        let is_nullable = self.is_nullable && Self::is_aidl_nullable(&array_info.value_type);

        if let ValueType::UserDefined(name) = &array_info.value_type {
            if let Some(lookup_decl) = lookup_decl_from_name(name, crate::Namespace::AIDL) {
                if matches!(&lookup_decl.decl, Declaration::Enum(_)) {
                    return self.init_enum_array_value(
                        expr,
                        &lookup_decl,
                        param,
                        array_info.is_fixed(),
                        is_nullable,
                    );
                }
            }
        }
        Ok(Self::init_array_value(expr, array_info, param, is_nullable))
    }

    /// Renders the initializer for a scalar (non-array) field/const default.
    /// Returns the bare initializer; the outer `Option` wrap (when nullable) is
    /// applied by `init_value`. Enum-typed targets are always primitive and
    /// thus never nullable, so the wrap is a no-op for them.
    fn init_scalar_branch(&self, expr: &ConstExpr, param: InitParam) -> Result<String, AidlError> {
        if let Some(enum_lookup) = self.enum_lookup() {
            return self.init_enum_value(expr, &enum_lookup, param);
        }

        let scalar_param = param.with_fixed_array(false).with_nullable(false);
        Ok(match expr.calculate() {
            Ok(calculated) => match &calculated.value {
                // An enum reference whose target is the enum type keeps the
                // symbolic member; a primitive target (e.g. int) takes its value.
                ValueType::Reference { value, .. } => {
                    if matches!(&self.value_type, ValueType::UserDefined(_)) {
                        calculated.value.to_init(scalar_param)
                    } else {
                        ValueType::Int64(*value).to_init(scalar_param)
                    }
                }
                _ => match calculated.convert_to(&self.value_type) {
                    Ok(converted) => converted.value.to_init(scalar_param),
                    Err(_) => calculated.value.to_init(scalar_param),
                },
            },
            Err(_) => ValueType::Void.to_init(scalar_param),
        })
    }

    pub(crate) fn init_value(
        &self,
        const_expr: Option<&ConstExpr>,
        param: InitParam,
    ) -> Result<String, AidlError> {
        let Some(expr) = const_expr else {
            return Ok(ValueType::Void.to_init(param.with_fixed_array(false).with_nullable(false)));
        };

        let init_str = if matches!(self.value_type, ValueType::Array(_)) {
            self.init_array_branch(expr, param)?
        } else {
            self.init_scalar_branch(expr, param)?
        };

        Ok(if self.is_nullable {
            format!("Some({init_str})")
        } else {
            init_str
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_type_declaration() {
        let gen = TypeGenerator::new(&NonArrayType {
            name: "String".to_owned(),
            generic: None,
            name_span: None,
        })
        .unwrap();

        assert_eq!(gen.type_declaration(false), "String");

        let nullable_gen = gen.clone().nullable().unwrap();
        assert_eq!(nullable_gen.type_declaration(false), "Option<String>");

        let array_gen = gen.array(&Vec::new());
        assert_eq!(array_gen.type_declaration(false), "Vec<String>");
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Out)
                .unwrap()
                .type_declaration(false),
            "Vec<String>"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Inout)
                .unwrap()
                .type_declaration(false),
            "Vec<String>"
        );

        let nullable_array_gen = array_gen.nullable().unwrap();
        assert_eq!(
            nullable_array_gen.type_declaration(false),
            "Option<Vec<Option<String>>>"
        );
        assert_eq!(
            nullable_array_gen
                .clone()
                .direction(&Direction::Out)
                .unwrap()
                .type_declaration(false),
            "Option<Vec<Option<String>>>"
        );
        assert_eq!(
            nullable_array_gen
                .direction(&Direction::Inout)
                .unwrap()
                .type_declaration(false),
            "Option<Vec<Option<String>>>"
        );
    }

    #[test]
    fn test_binder_declaration() {
        let gen = TypeGenerator::new(&NonArrayType {
            name: "IBinder".to_owned(),
            generic: None,
            name_span: None,
        })
        .unwrap();

        assert_eq!(gen.type_declaration(false), "rsbinder::SIBinder");

        let nullable_gen = gen.clone().nullable().unwrap();
        assert_eq!(
            nullable_gen.type_declaration(false),
            "Option<rsbinder::SIBinder>"
        );

        let array_gen = gen.array(&Vec::new());
        assert_eq!(array_gen.type_declaration(false), "Vec<rsbinder::SIBinder>");
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Out)
                .unwrap()
                .type_declaration(false),
            "Vec<Option<rsbinder::SIBinder>>"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Inout)
                .unwrap()
                .type_declaration(false),
            "Vec<Option<rsbinder::SIBinder>>"
        );

        let nullable_array_gen = array_gen.nullable().unwrap();
        assert_eq!(
            nullable_array_gen.type_declaration(false),
            "Option<Vec<Option<rsbinder::SIBinder>>>"
        );
        assert_eq!(
            nullable_array_gen
                .clone()
                .direction(&Direction::Out)
                .unwrap()
                .type_declaration(false),
            "Option<Vec<Option<rsbinder::SIBinder>>>"
        );
        assert_eq!(
            nullable_array_gen
                .direction(&Direction::Inout)
                .unwrap()
                .type_declaration(false),
            "Option<Vec<Option<rsbinder::SIBinder>>>"
        );
    }

    #[test]
    fn test_type_decl_for_func() {
        let gen = TypeGenerator::new(&NonArrayType {
            name: "ParcelFileDescriptor".to_owned(),
            generic: None,
            name_span: None,
        })
        .unwrap();

        assert_eq!(
            gen.type_decl_for_func().unwrap(),
            "&rsbinder::ParcelFileDescriptor"
        );

        let nullable_gen = gen.clone().nullable().unwrap();
        assert_eq!(
            nullable_gen.type_decl_for_func().unwrap(),
            "Option<&rsbinder::ParcelFileDescriptor>"
        );

        let array_gen = gen.array(&Vec::new());
        assert_eq!(
            array_gen.type_decl_for_func().unwrap(),
            "&[rsbinder::ParcelFileDescriptor]"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Out)
                .unwrap()
                .type_decl_for_func()
                .unwrap(),
            "&mut Vec<Option<rsbinder::ParcelFileDescriptor>>"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Inout)
                .unwrap()
                .type_decl_for_func()
                .unwrap(),
            "&mut Vec<rsbinder::ParcelFileDescriptor>"
        );

        let nullable_array_gen = array_gen.nullable().unwrap();
        assert_eq!(
            nullable_array_gen.type_decl_for_func().unwrap(),
            "Option<&[Option<rsbinder::ParcelFileDescriptor>]>"
        );
        assert_eq!(
            nullable_array_gen
                .clone()
                .direction(&Direction::Out)
                .unwrap()
                .type_decl_for_func()
                .unwrap(),
            "&mut Option<Vec<Option<rsbinder::ParcelFileDescriptor>>>"
        );
        assert_eq!(
            nullable_array_gen
                .direction(&Direction::Inout)
                .unwrap()
                .type_decl_for_func()
                .unwrap(),
            "&mut Option<Vec<Option<rsbinder::ParcelFileDescriptor>>>"
        );

        let gen = TypeGenerator::new(&NonArrayType {
            name: "boolean".to_owned(),
            generic: None,
            name_span: None,
        })
        .unwrap();
        let array_gen = gen.array(&Vec::new());
        assert_eq!(
            array_gen
                .direction(&Direction::Out)
                .unwrap()
                .type_decl_for_func()
                .unwrap(),
            "&mut Vec<bool>"
        );

        // ITestService.aidl
        // fn ReverseUtf8CppStringList(&self, _arg_input: Option<&[Option<String>]>, _arg_repeated: &mut Option<Vec<Option<String>>>) -> binder::Result<Option<Vec<Option<String>>>>;
        let gen = TypeGenerator::new(&NonArrayType {
            name: "String".to_owned(),
            generic: None,
            name_span: None,
        })
        .unwrap();
        let nullable_array_gen = gen.array(&Vec::new()).nullable().unwrap();
        assert_eq!(
            nullable_array_gen.type_decl_for_func().unwrap(),
            "Option<&[Option<String>]>"
        );
    }

    #[test]
    fn test_func_call_param() {
        let gen = TypeGenerator::new(&NonArrayType {
            name: "String".to_owned(),
            generic: None,
            name_span: None,
        })
        .unwrap()
        .identifier("type");
        assert_eq!(gen.func_call_param(), "_arg_type.as_str()");
        assert_eq!(
            gen.nullable().unwrap().func_call_param(),
            "_arg_type.as_deref()"
        );

        let gen = TypeGenerator::new(&NonArrayType {
            name: "ParcelFileDescriptor".to_owned(),
            generic: None,
            name_span: None,
        })
        .unwrap()
        .identifier("type");
        assert_eq!(gen.func_call_param(), "&_arg_type");

        let array_gen = gen.array(&Vec::new());
        assert_eq!(
            array_gen.clone().nullable().unwrap().func_call_param(),
            "_arg_type.as_deref()"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Out)
                .unwrap()
                .func_call_param(),
            "&mut _arg_type"
        );
        assert_eq!(
            array_gen
                .direction(&Direction::Inout)
                .unwrap()
                .func_call_param(),
            "&mut _arg_type"
        );
    }

    #[test]
    fn test_type_decl_for_struct() {
        let gen = TypeGenerator::new(&NonArrayType {
            name: "boolean".to_owned(),
            generic: None,
            name_span: None,
        })
        .unwrap()
        .identifier("type");
        let array_nullable = gen
            .array(&[ArrayType {
                const_expr: Some(ConstExpr::new(ValueType::Byte(2))),
            }])
            .nullable()
            .unwrap();
        assert_eq!(array_nullable.type_declaration(true), "Option<[bool; 2]>");
    }
}
