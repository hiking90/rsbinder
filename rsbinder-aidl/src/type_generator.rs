// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use miette::{NamedSource, SourceSpan};

use crate::const_expr::{ConstExpr, InitParam, ValueType};
use crate::error::{AidlError, ResolutionError, SemanticError};
use crate::parser::{self, *};

/// `Option` as generated code names it: by path, since an AIDL nested type may take the bare
/// name in the module the code lands in (AOSP `aidl_to_rust.cpp` paths `Vec`/`Box`/`String`).
pub const OPTION: &str = "::core::option::Option";
/// `Vec` as generated code names it; see [`OPTION`].
pub const VEC: &str = "::std::vec::Vec";
/// `Box` as generated code names it; see [`OPTION`].
pub const BOX: &str = "::std::boxed::Box";
/// `String` as generated code names it; see [`OPTION`].
pub const STRING: &str = "::std::string::String";

/// Source and span for a type-level diagnostic; placeholder name without source context.
fn diagnostic_source(span: Option<(usize, usize)>) -> (NamedSource<String>, SourceSpan) {
    let filename = parser::current_source_name();
    let source = parser::current_source_text();
    let (start, end) = span.unwrap_or((0, 0));
    // Clamp against miette `OutOfBounds`; with no source, keep raw offsets for API consumers.
    let (start, end) = if source.is_empty() {
        (start, end)
    } else {
        let start = start.min(source.len());
        (start, end.clamp(start, source.len()))
    };
    let src_name = if filename.is_empty() {
        "<type_generator>".to_string()
    } else {
        filename
    };
    (
        NamedSource::new(src_name, source),
        SourceSpan::new(start.into(), end - start),
    )
}

/// AOSP `FormatDirections` (`aidl_language.cpp:1072`): `in`, `in or inout`, `in, out, or inout`.
fn format_directions(directions: &[&str]) -> String {
    match directions {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [a, b] => format!("{a} or {b}"),
        [init @ .., last] => format!("{}, or {last}", init.join(", ")),
    }
}

fn format_directions_quoted(directions: &[&str]) -> String {
    let quoted: Vec<String> = directions.iter().map(|d| format!("'{d}'")).collect();
    let quoted: Vec<&str> = quoted.iter().map(String::as_str).collect();
    format_directions(&quoted)
}

fn make_type_error(message: impl Into<String>, span: Option<(usize, usize)>) -> AidlError {
    let (src, span) = diagnostic_source(span);
    AidlError::from(SemanticError::InvalidOperation {
        message: message.into(),
        src,
        span,
    })
}

/// AIDL name of a `Reference`'s enum: `enum_type`, or `<Union>.Tag` for a union's `Tag`.
fn enum_aidl_name(enum_type: &str, enum_name: &str) -> String {
    match parser::lookup_decl_from_canonical(enum_type) {
        Some(found) if matches!(found.decl, Declaration::Union(_)) => {
            format!("{enum_type}.{enum_name}")
        }
        _ => enum_type.to_owned(),
    }
}

thread_local! {
    // Thread-local like the parser.rs compiler state; only `Generator::declarations` sets it.
    static IS_CRATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn crate_name() -> &'static str {
    if IS_CRATE.with(|c| c.get()) {
        "crate"
    } else {
        "rsbinder"
    }
}

pub fn set_crate_support(support: bool) {
    IS_CRATE.with(|c| c.set(support));
}

#[derive(Clone, Debug)]
struct ArrayInfo {
    sizes: Vec<i64>,
    value_type: ValueType,
    is_list: bool,
}

impl ArrayInfo {
    fn new(
        value_type: &ValueType,
        array_types: &[parser::ArrayType],
        span: Option<(usize, usize)>,
    ) -> Result<Self, AidlError> {
        let mut sizes = Vec::with_capacity(array_types.len());
        for t in array_types {
            match &t.const_expr {
                // `T[]` — variable-length dimension; 0 marks "not fixed".
                None => sizes.push(0),
                Some(expr) => {
                    // AOSP rejects bad/negative/non-integral sizes; 0 is our "not fixed" sentinel.
                    let calculated = expr.calculate().map_err(|e| {
                        make_type_error(
                            format!("cannot evaluate fixed-size array dimension: {}", e.message),
                            span,
                        )
                    })?;
                    let size = match &calculated.value {
                        ValueType::Byte(_)
                        | ValueType::Int32(_)
                        | ValueType::Int64(_)
                        | ValueType::Reference { .. } => calculated.to_i64().map_err(|e| {
                            make_type_error(
                                format!(
                                    "cannot evaluate fixed-size array dimension: {}",
                                    e.message
                                ),
                                span,
                            )
                        })?,
                        other => {
                            return Err(make_type_error(
                                format!(
                                    "fixed-size array dimension must be an integral constant \
                                     (got {})",
                                    other.to_value_string()
                                ),
                                span,
                            ))
                        }
                    };
                    if size <= 0 || size > i32::MAX as i64 {
                        return Err(make_type_error(
                            format!(
                                "fixed-size array dimension must be a positive constant that \
                                 fits in int (got {size})"
                            ),
                            span,
                        ));
                    }
                    sizes.push(size);
                }
            }
        }
        Ok(Self {
            sizes,
            value_type: value_type.clone(),
            is_list: false,
        })
    }

    fn new_list(
        value_type: &ValueType,
        array_types: &[parser::ArrayType],
        span: Option<(usize, usize)>,
    ) -> Result<Self, AidlError> {
        let mut this = Self::new(value_type, array_types, span)?;
        this.is_list = true;
        Ok(this)
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
    /// User-defined generic's arguments, bare or as array/`List` element; empty otherwise.
    type_args: Vec<TypeGenerator>,
    pub identifier: String,
    direction: Direction,
    type_span: Option<(usize, usize)>,
}

impl TypeGenerator {
    pub fn new(aidl_type: &NonArrayType) -> Result<Self, AidlError> {
        let mut array_types = Vec::new();
        let mut type_args = Vec::new();
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
                    // `List<int[]>` parses but would hit `type_decl`'s panic; AOSP rejects it too.
                    let args = gen.type_args();
                    if args.len() != 1 {
                        return Err(make_type_error(
                            format!(
                                "List can only have one type parameter, but got {}",
                                args.len()
                            ),
                            aidl_type.name_span,
                        ));
                    }
                    let elem_generator = Self::new_with_type(&args[0])?;
                    // `List<Foo<int>>`: the list carries its element's type arguments.
                    type_args = elem_generator.type_args;
                    let elem = elem_generator.value_type;
                    if matches!(elem, ValueType::Array(_)) {
                        return Err(make_type_error(
                            "List element type cannot be an array",
                            aidl_type.name_span,
                        ));
                    }
                    // No `SerializeArray` for these; AOSP `kListUsage` (aidl_language.cpp) rejects.
                    if matches!(elem, ValueType::Void) {
                        return Err(make_type_error(
                            "List element type cannot be void",
                            aidl_type.name_span,
                        ));
                    }
                    if matches!(elem, ValueType::Holder) {
                        return Err(make_type_error(
                            "List element type cannot be ParcelableHolder",
                            aidl_type.name_span,
                        ));
                    }
                    array_types.push(ArrayInfo::new_list(
                        &elem,
                        &Vec::new(),
                        aidl_type.name_span,
                    )?);
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

        // AOSP `AidlTypeSpecifier::CheckValid`: only List (Array here) and UserDefined are generic.
        if aidl_type.generic.is_some()
            && !matches!(value_type, ValueType::Array(_) | ValueType::UserDefined(_))
        {
            return Err(make_type_error(
                format!("'{}' is not a generic type", aidl_type.name),
                aidl_type.name_span,
            ));
        }

        // Arity and requirements are checked later, in `ensure_resolvable` at every use site.
        if let (ValueType::UserDefined(_), Some(generic)) = (&value_type, &aidl_type.generic) {
            for arg in generic.type_args() {
                type_args.push(Self::new_with_type(&arg)?);
            }
        }

        Ok(Self {
            is_nullable: false,
            value_type,
            array_types,
            type_args,
            identifier: String::new(),
            direction: Default::default(),
            type_span: aidl_type.name_span,
        })
    }

    pub fn new_with_type(_type: &Type) -> Result<Self, AidlError> {
        let mut this = Self::new(&_type.non_array_type)?;

        let is_nullable = has_annotation(&_type.annotation_list, AnnotationType::IsNullable);
        let is_array = !_type.array_types.is_empty();

        // AOSP `CheckValid`: void only bare, ParcelableHolder never array or nullable.
        if matches!(this.value_type, ValueType::Void) && (is_array || is_nullable) {
            return Err(make_type_error(
                "void type cannot be an array or nullable",
                _type.non_array_type.name_span,
            ));
        }
        if matches!(this.value_type, ValueType::Holder) {
            if is_array {
                return Err(make_type_error(
                    "arrays of ParcelableHolder are not supported",
                    _type.non_array_type.name_span,
                ));
            }
            if is_nullable {
                return Err(make_type_error(
                    "ParcelableHolder cannot be nullable",
                    _type.non_array_type.name_span,
                ));
            }
        }

        if !_type.array_types.is_empty() {
            // `List<T>[]`: `array()` would drop the `[]`; AOSP rejects arrays of lists.
            if matches!(this.value_type, ValueType::Array(_)) {
                return Err(make_type_error(
                    "an array of List is not supported",
                    _type.non_array_type.name_span,
                ));
            }
            // AOSP: multi-dim arrays must be fixed in every dim, or `int[][]` becomes `Vec<i32>`.
            if _type.array_types.len() > 1
                && _type.array_types.iter().any(|a| a.const_expr.is_none())
            {
                return Err(make_type_error(
                    "a variable-length array must be one-dimensional \
                     (multi-dimensional arrays must be fixed-size)",
                    _type.non_array_type.name_span,
                ));
            }
            this = this.array(&_type.array_types)?;
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
        let check = |value_type: &ValueType| -> Result<Option<LookupDecl>, AidlError> {
            if let ValueType::UserDefined(name) = value_type {
                let resolved = lookup_decl_from_name(name, crate::Namespace::AIDL);
                if resolved.is_none() {
                    let (src, span) = diagnostic_source(self.type_span);
                    return Err(AidlError::from(ResolutionError::UnknownType {
                        name: name.clone(),
                        src,
                        span,
                    }));
                }
                return Ok(resolved);
            }
            Ok(None)
        };

        let mut named = check(&self.value_type)?;
        for array_info in &self.array_types {
            if let Some(found) = check(&array_info.value_type)? {
                named = Some(found);
            }
        }
        for arg in &self.type_args {
            arg.ensure_resolvable()?;
        }
        match named {
            Some(lookup_decl) => self.ensure_type_args(&lookup_decl),
            None => Ok(()),
        }
    }

    /// AOSP `AidlTypeSpecifier::CheckValid` for a user-defined generic's arguments.
    fn ensure_type_args(&self, lookup_decl: &LookupDecl) -> Result<(), AidlError> {
        let params: &[parser::TypeParam] = match &lookup_decl.decl {
            Declaration::Parcelable(decl) => &decl.type_params,
            Declaration::Union(decl) => &decl.type_params,
            _ => &[],
        };
        let name = lookup_decl.decl.name();
        if params.is_empty() {
            if !self.type_args.is_empty() {
                return Err(make_type_error(
                    format!("'{name}' is not a generic type"),
                    self.type_span,
                ));
            }
            return Ok(());
        }
        if params.len() != self.type_args.len() {
            return Err(make_type_error(
                format!(
                    "'{name}' must have {} type parameters, but got {}",
                    params.len(),
                    self.type_args.len()
                ),
                self.type_span,
            ));
        }
        for (param, arg) in params.iter().zip(&self.type_args) {
            // AOSP `GetRustName` drops `[]`, mis-names `List`; refusing `void` is an rsbinder rule.
            let unsupported = match &arg.value_type {
                ValueType::Array(_) => Some("an array or List"),
                ValueType::Void => Some("void"),
                _ => None,
            };
            if let Some(what) = unsupported {
                return Err(make_type_error(
                    format!("'{name}': a type argument cannot be {what}"),
                    arg.type_span,
                ));
            }
            // AOSP reads every parameter annotation as a requirement; any other is never met.
            for requirement in &param.annotation_list {
                let satisfied = match requirement.annotation.as_str() {
                    "@FixedSize" => arg.can_be_fixed_size(),
                    "@VintfStability" => arg.is_vintf_declaration(),
                    _ => false,
                };
                if !satisfied {
                    return Err(make_type_error(
                        format!(
                            "type '{}' used as type parameter '{}' of '{name}' must be \
                             annotated with {}",
                            arg.aidl_name(),
                            param.name,
                            requirement.annotation
                        ),
                        arg.type_span,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Whether this is a bare user-defined type in `@VintfStability` scope (own or enclosing).
    fn is_vintf_declaration(&self) -> bool {
        if !self.array_types.is_empty() {
            return false;
        }
        match &self.value_type {
            ValueType::UserDefined(name) => lookup_decl_from_name(name, crate::Namespace::AIDL)
                .is_some_and(|lookup| parser::is_vintf_scoped(&lookup.ns)),
            _ => false,
        }
    }

    /// The type as the AIDL author wrote its head, for diagnostics.
    fn aidl_name(&self) -> String {
        fn head(value_type: &ValueType) -> String {
            match value_type {
                ValueType::Bool(_) => "boolean".into(),
                ValueType::Byte(_) => "byte".into(),
                ValueType::Char(_) => "char".into(),
                ValueType::Int32(_) => "int".into(),
                ValueType::Int64(_) => "long".into(),
                ValueType::Float(_) => "float".into(),
                ValueType::Double(_) => "double".into(),
                ValueType::String(_) => "String".into(),
                ValueType::IBinder => "IBinder".into(),
                ValueType::FileDescriptor => "ParcelFileDescriptor".into(),
                ValueType::Holder => "ParcelableHolder".into(),
                ValueType::Void => "void".into(),
                ValueType::UserDefined(name) => name.clone(),
                other => other.to_value_string(),
            }
        }
        let name = match &self.value_type {
            ValueType::Array(_) => self
                .array_types
                .first()
                .map(|info| format!("{}[]", head(&info.value_type)))
                .unwrap_or_default(),
            other => head(other),
        };
        if self.is_nullable {
            format!("@nullable {name}")
        } else {
            name
        }
    }

    /// Reject a field that closes a reference cycle without a form that can
    /// terminate.
    ///
    /// `@nullable` renders such a field as `Option<Box<T>>`, which both breaks
    /// the cycle and gives the generated `Default` impl somewhere to stop. A
    /// non-nullable one has neither: boxing it alone would produce a `Default`
    /// that recurses until the stack runs out, and that `Default` is the
    /// deserialization entry point, so a peer's parcel would abort the
    /// process. Only fields no box can break count here
    /// (`SizingEdges::Unboxable`): once a `@nullable` field on a cycle is boxed,
    /// the other fields on it stay inline, as AOSP `CheckNoRecursiveDefinition`
    /// accepts for `@nullable(heap=true)`. Must be invoked while the owning
    /// declaration's `NamespaceGuard` is active.
    pub fn ensure_sized(&self) -> Result<(), AidlError> {
        // Only bare fields (`@nullable` rescues) and inline `[T; N]` (no Box codec) close cycles.
        let (type_name, nullable_rescues) = match &self.value_type {
            ValueType::UserDefined(name) => (name, true),
            ValueType::Array(_) => match self.array_types.first() {
                Some(info) if info.is_fixed() => match &info.value_type {
                    ValueType::UserDefined(name) => (name, false),
                    _ => return Ok(()),
                },
                _ => return Ok(()),
            },
            _ => return Ok(()),
        };
        if nullable_rescues && self.is_nullable {
            return Ok(());
        }
        let Some(lookup_decl) = lookup_decl_from_name(type_name, crate::Namespace::AIDL) else {
            return Ok(());
        };
        if !Self::closes_reference_cycle(&lookup_decl, SizingEdges::Unboxable) {
            return Ok(());
        }
        let (src, span) = diagnostic_source(self.type_span);
        let help = if nullable_rescues {
            "mark the field `@nullable` so it becomes `Option<Box<…>>`, which \
             breaks the cycle and can still be default-constructed"
        } else {
            "a fixed-size array keeps its elements inline; use a \
             variable-length array (`T[]`) so the elements live behind a `Vec`"
        };
        Err(AidlError::from(SemanticError::RecursiveParcelable {
            type_name: type_name.clone(),
            help: Some(help.to_owned()),
            src,
            span,
        }))
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

    // Only a parcelable or union is held inline; interfaces are handles and enums are scalars.
    fn closes_reference_cycle(lookup_decl: &crate::parser::LookupDecl, edges: SizingEdges) -> bool {
        if !matches!(
            lookup_decl.decl,
            Declaration::Parcelable(_) | Declaration::Union(_)
        ) {
            return false;
        }
        let curr_ns = current_namespace();
        let refers_to_self = curr_ns.relative_mod(&lookup_decl.ns).is_empty()
            && lookup_decl
                .name
                .ns
                .last()
                .is_some_and(|name| curr_ns.ns.last() == Some(name));
        refers_to_self || crate::parser::declaration_reaches(&lookup_decl.ns, &curr_ns, edges)
    }

    /// `allow_box` is false for array elements: `Box<T>` has no `SerializeArray` impl.
    fn make_user_defined_type_name(&self, type_name: &str, allow_box: bool) -> String {
        let lookup_decl = lookup_decl_from_name(type_name, crate::Namespace::AIDL)
            .expect("type must be resolved during code generation");
        let curr_ns = current_namespace();
        let ns = curr_ns.relative_mod(&lookup_decl.ns);
        // AIDL allows Rust keywords as names; `relative_mod` already escaped the module path.
        let simple = crate::escape_rust_keyword(lookup_decl.name.ns.last().unwrap());
        let is_interface = matches!(lookup_decl.decl, Declaration::Interface(_));
        // Only `@nullable` boxes: `Option` ends `Default` recursion; `ensure_sized` rejects others.
        let needs_box = allow_box
            && self.is_nullable
            && !is_interface
            && Self::closes_reference_cycle(&lookup_decl, SizingEdges::ByValue);
        // A builtin is the runtime crate's type, not a module of this output.
        let path = if let Some(builtin) = parser::builtin_rust_path(&lookup_decl.ns) {
            format!("{}::{builtin}", crate_name())
        } else if !ns.is_empty() {
            format!("{ns}::{simple}")
        } else {
            simple.into_owned()
        };
        // AOSP `GetRustName` names an argument bare: `Option`/`Vec`/`&` wrap only the whole type.
        let path = if self.type_args.is_empty() {
            path
        } else {
            let args: Vec<String> = self
                .type_args
                .iter()
                .map(|arg| arg.type_arg_decl())
                .collect();
            format!("{path}<{}>", args.join(", "))
        };
        let name = if needs_box {
            format!("{BOX}<{path}>")
        } else {
            path
        };

        if is_interface {
            format!("{}::Strong<dyn {}>", crate_name(), name)
        } else {
            name
        }
    }

    /// Bare Rust name as a type argument; the parser refuses `@nullable` there.
    fn type_arg_decl(&self) -> String {
        self.type_decl(&self.value_type, false)
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

    // AOSP `UsesOptionInNullableVector`: primitives and enums stay bare (no null-marker word).
    fn nullable_element(value_type: &ValueType, type_name: &str) -> String {
        if Self::is_primitive(value_type) {
            type_name.to_owned()
        } else {
            format!("{OPTION}<{type_name}>")
        }
    }

    /// Can a field of this type appear in a `@FixedSize` parcelable or union?
    ///
    /// Ports AOSP `AidlTypenames::CanBeFixedSize` (`aidl_typenames.cpp`): a
    /// generic (`List<T>` or `Foo<T>`, whatever `Foo` is annotated), a
    /// `@nullable` type, and a variable-length array are all variable size;
    /// primitives and enums are fixed; a parcelable or union is fixed only if
    /// it is itself `@FixedSize`; every other builtin (`String`, `IBinder`,
    /// `ParcelFileDescriptor`, `ParcelableHolder`) and every interface handle
    /// is not. Must be invoked while the owning declaration's
    /// `NamespaceGuard` is active: a `UserDefined` name that fails to resolve
    /// is reported as non-fixed, which would reject valid input.
    pub fn can_be_fixed_size(&self) -> bool {
        if self.is_nullable || !self.type_args.is_empty() {
            return false;
        }
        let element = match self.array_types.first() {
            Some(info) => {
                if info.is_list || !info.is_fixed() {
                    return false;
                }
                &info.value_type
            }
            None => &self.value_type,
        };
        Self::value_type_can_be_fixed_size(element)
    }

    fn value_type_can_be_fixed_size(value_type: &ValueType) -> bool {
        match value_type {
            ValueType::Bool(_)
            | ValueType::Byte(_)
            | ValueType::Char(_)
            | ValueType::Int32(_)
            | ValueType::Int64(_)
            | ValueType::Float(_)
            | ValueType::Double(_) => true,
            ValueType::UserDefined(name) => {
                match lookup_decl_from_name(name, crate::Namespace::AIDL) {
                    Some(lookup_decl) => match &lookup_decl.decl {
                        Declaration::Enum(_) => true,
                        // `@FixedSize` is not scoped: nested declarations do not inherit it.
                        Declaration::Parcelable(decl) => {
                            has_annotation(&decl.annotation_list, AnnotationType::FixedSize)
                        }
                        Declaration::Union(decl) => {
                            has_annotation(&decl.annotation_list, AnnotationType::FixedSize)
                        }
                        _ => false,
                    },
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// Every user-defined type this generator names: the type itself, or the
    /// element type when it is an array or a `List<T>`. Used to walk a
    /// `@VintfStability` declaration's reference closure.
    pub fn referenced_user_types(&self) -> Vec<&str> {
        let mut names: Vec<&str> = std::iter::once(&self.value_type)
            .chain(self.array_types.iter().map(|info| &info.value_type))
            .filter_map(|value_type| match value_type {
                ValueType::UserDefined(name) => Some(name.as_str()),
                _ => None,
            })
            .collect();
        // A type argument is part of the signature too (`Foo<Bar>` names `Bar`).
        for arg in &self.type_args {
            names.extend(arg.referenced_user_types());
        }
        names
    }

    /// Can a `const` have this type?
    ///
    /// AOSP `AidlConstantDeclaration::CheckValid` admits exactly `{String,
    /// byte, int, long, float, double}`. rsbinder deliberately allows more —
    /// `boolean`, `char`, and constant arrays of those — and its test suite
    /// pins that, so this enforces the part of AOSP's rule that is not an
    /// extension: a constant's type must be one with a constant *form*.
    ///
    /// A handle (`IBinder`, `ParcelFileDescriptor`, `ParcelableHolder`) has no
    /// literal to be initialized from, and a user-defined type is refused by
    /// AOSP outright — which is also why the `@VintfStability` reference
    /// closure need not walk constants: a constant can never name a type that
    /// would have to be VINTF-stable.
    pub fn is_supported_constant_type(&self) -> bool {
        let element = match self.array_types.first() {
            Some(info) => &info.value_type,
            None => &self.value_type,
        };
        matches!(
            element,
            ValueType::Bool(_)
                | ValueType::Byte(_)
                | ValueType::Char(_)
                | ValueType::Int32(_)
                | ValueType::Int64(_)
                | ValueType::Float(_)
                | ValueType::Double(_)
                | ValueType::String(_)
        )
    }

    /// A bare `void`, or a `List<void>` whose element is one. Legal only as a
    /// method return type — AOSP rejects it for parameters (`aidl_language.cpp`
    /// `AidlMethod::CheckValid`), for field/constant declarations
    /// (`AidlVariableDeclaration::CheckValid`), and as a `List` element
    /// (`AidlTypeSpecifier::CheckValid`); `void[]` is rejected earlier, in
    /// [`Self::new_with_type`], and `List<void>` in [`Self::new`].
    pub fn is_void(&self) -> bool {
        matches!(self.value_type, ValueType::Void)
            || self
                .array_types
                .first()
                .is_some_and(|info| matches!(info.value_type, ValueType::Void))
    }

    /// A bare `ParcelableHolder`. AOSP rejects it as a method argument, as a
    /// method return type, and as a union member (`aidl_typenames.cpp`
    /// `AidlTypenames::GetArgumentAspect`, which gives it an empty direction
    /// set; `aidl_language.cpp` `AidlArgument::CheckValid`,
    /// `AidlMethod::CheckValid`, `AidlUnionDecl::CheckValid`). The array form
    /// is rejected earlier, in [`Self::new_with_type`], and the
    /// `List<ParcelableHolder>` form in [`Self::new`].
    ///
    /// The predicate answers only "is the value type a holder", so it stays
    /// true for a nullable one. That combination is reachable: the field-level
    /// `@nullable ParcelableHolder` form is rejected in
    /// [`Self::new_with_type`], but a method-level `@nullable` is applied
    /// after construction (`generator.rs` calls [`Self::nullable_at`] on the
    /// finished generator), and `nullable_at` rejects primitives only.
    pub fn is_parcelable_holder(&self) -> bool {
        matches!(self.value_type, ValueType::Holder)
    }

    /// Span of the type as written, for diagnostics raised by callers that
    /// only hold the generator.
    pub fn type_span(&self) -> Option<(usize, usize)> {
        self.type_span
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
                            // `Strong<dyn IFoo>` has no Default: fields are `Option<Strong<_>>`.
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

    /// AOSP `GetArgumentAspect` (aidl_typenames.cpp:332): (type name, permitted directions).
    fn argument_aspect(&self) -> Option<(&'static str, &'static [&'static str])> {
        const ALL: &[&str] = &["in", "out", "inout"];
        const IN: &[&str] = &["in"];
        Some(match &self.value_type {
            ValueType::Array(_) => match self.array_types.first() {
                Some(info) if info.is_list => ("List", ALL),
                _ => ("array", ALL),
            },
            // Not default-constructible, so no `out`.
            ValueType::FileDescriptor => ("ParcelFileDescriptor", &["in", "inout"]),
            ValueType::Holder => return None,
            ValueType::IBinder => ("IBinder", IN),
            ValueType::Void => ("void", IN),
            ValueType::Bool(_) => ("boolean", IN),
            ValueType::Byte(_) => ("byte", IN),
            ValueType::Char(_) => ("char", IN),
            ValueType::Int32(_) => ("int", IN),
            ValueType::Int64(_) => ("long", IN),
            ValueType::Float(_) => ("float", IN),
            ValueType::Double(_) => ("double", IN),
            ValueType::String(_) => ("String", IN),
            ValueType::UserDefined(name) => {
                let is_immutable = |annotations: &[Annotation]| {
                    annotations
                        .iter()
                        .any(|a| a.annotation == "@JavaOnlyImmutable")
                };
                let found = lookup_decl_from_name(name, crate::Namespace::AIDL)?;
                match found.decl {
                    Declaration::Parcelable(decl) if is_immutable(&decl.annotation_list) => {
                        ("@JavaOnlyImmutable", IN)
                    }
                    Declaration::Union(decl) if is_immutable(&decl.annotation_list) => {
                        ("@JavaOnlyImmutable", IN)
                    }
                    Declaration::Parcelable(_) | Declaration::Union(_) => ("parcelable/union", ALL),
                    Declaration::Interface(_) => ("interface", IN),
                    Declaration::Enum(_) => ("enum", IN),
                    Declaration::Variable(_) => return None,
                }
            }
            _ => return None,
        })
    }

    /// Sets an argument's direction, checked as AOSP `AidlArgument::CheckValid`.
    pub fn direction_at(
        mut self,
        direction: &Direction,
        direction_span: Option<(usize, usize)>,
        arg: &str,
    ) -> Result<Self, AidlError> {
        if let Some((type_kind, allowed)) = self.argument_aspect() {
            let given = match direction {
                Direction::None => None,
                Direction::In => Some("in"),
                Direction::Out => Some("out"),
                Direction::Inout => Some("inout"),
            };
            let allowed_text = format_directions(allowed);
            match given {
                None if allowed != ["in"] => {
                    let (src, span) = diagnostic_source(self.type_span);
                    return Err(AidlError::from(SemanticError::DirectionNotSpecified {
                        arg: arg.to_owned(),
                        type_kind: type_kind.to_owned(),
                        help: Some(format!(
                            "declare it as {} before the type",
                            format_directions_quoted(allowed)
                        )),
                        allowed: allowed_text,
                        src,
                        span,
                    }));
                }
                Some(dir) if !allowed.contains(&dir) => {
                    let help = if allowed == ["in"] {
                        format!(
                            "remove '{dir}'; to pass a value back, return it, or use an array, \
                             which can be an in, out, or inout parameter"
                        )
                    } else {
                        let others: Vec<&str> =
                            allowed.iter().copied().filter(|d| *d != "in").collect();
                        format!(
                            "remove '{dir}', or declare it as {}",
                            format_directions_quoted(&others)
                        )
                    };
                    let (src, span) = diagnostic_source(direction_span.or(self.type_span));
                    return Err(AidlError::from(SemanticError::InvalidDirection {
                        arg: arg.to_owned(),
                        direction: dir.to_owned(),
                        type_kind: type_kind.to_owned(),
                        allowed: allowed_text,
                        help: Some(help),
                        src,
                        span,
                    }));
                }
                _ => {}
            }
        }
        self.direction = direction.clone();
        Ok(self)
    }

    /// Sets the direction unchecked, for code generation; arguments go through `direction_at`.
    pub fn direction(mut self, direction: &Direction) -> Result<Self, AidlError> {
        self.direction = direction.clone();
        Ok(self)
    }

    // Switch to array type.
    pub fn array(mut self, array_types: &[parser::ArrayType]) -> Result<Self, AidlError> {
        match self.value_type {
            ValueType::Array(_) => Ok(self),
            _ => {
                self.array_types.push(ArrayInfo::new(
                    &self.value_type,
                    array_types,
                    self.type_span,
                )?);
                self.value_type = ValueType::Array(Vec::new());
                Ok(self)
            }
        }
    }

    fn array_type_name(&self, value_type: &ValueType) -> String {
        let name = self.type_decl(value_type, false);
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
                format!("{OPTION}<{type_name}>")
            } else {
                type_name
            }
        } else {
            // AOSP `RustNameOf` (aidl_to_rust.cpp:253-276): only `out` elements need `Default`.
            if matches!(self.direction, Direction::Out)
                && !Self::can_be_defaulted(&array_info.value_type, is_struct)
            {
                format!("{OPTION}<{type_name}>")
            } else if self.is_nullable {
                Self::nullable_element(&array_info.value_type, &type_name)
            } else {
                type_name
            }
        };

        array_info.sizes.iter().rev().skip(1).fold(
            format!("[{}; {}]", value_str, array_info.sizes.last().unwrap()),
            |acc, size| format!("[{acc}; {size}]"),
        )
    }

    fn list_type_decl_fixed(&self, array_info: &ArrayInfo, is_struct: bool) -> String {
        // Fixed-size array wrapping ignores direction; only nullability adds `Option<_>`.
        let fixed_array = self.make_fixed_array(array_info, is_struct);
        if self.is_nullable {
            format!("{OPTION}<{fixed_array}>")
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
                    format!(
                        "{VEC}<{}>",
                        Self::nullable_element(&sub_type.value_type, &type_name)
                    )
                } else if Self::can_be_defaulted(&sub_type.value_type, is_struct) {
                    format!("{VEC}<{type_name}>")
                } else {
                    format!("{VEC}<{OPTION}<{type_name}>>")
                }
            }
            Direction::Inout => {
                if self.is_nullable {
                    format!(
                        "{VEC}<{}>",
                        Self::nullable_element(&sub_type.value_type, &type_name)
                    )
                } else {
                    // AOSP `RustNameOf` INOUT: read fully populated, no element needs `Default`.
                    format!("{VEC}<{type_name}>")
                }
            }
            _ => {
                if is_struct {
                    if self.is_nullable && Self::is_aidl_nullable(&sub_type.value_type) {
                        format!("{VEC}<{OPTION}<{type_name}>>")
                    } else {
                        format!("{VEC}<{type_name}>")
                    }
                } else if self.is_nullable {
                    if Self::is_primitive(&sub_type.value_type) {
                        format!("{OPTION}<{VEC}<{type_name}>>")
                    } else {
                        format!("{OPTION}<{VEC}<{OPTION}<{type_name}>>>")
                    }
                } else {
                    format!("{VEC}<{type_name}>")
                }
            }
        }
    }

    fn type_decl(&self, value_type: &ValueType, allow_box: bool) -> String {
        match value_type {
            ValueType::Void => "()".into(),
            ValueType::String(_) => STRING.into(),
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
            }
            ValueType::IBinder => format!("{}::SIBinder", crate_name()),
            ValueType::FileDescriptor => format!("{}::ParcelFileDescriptor", crate_name()),
            ValueType::Holder => format!("{}::ParcelableHolder", crate_name()),
            ValueType::UserDefined(name) => self.make_user_defined_type_name(name, allow_box),
            _ => unreachable!(),
        }
    }

    pub fn type_declaration(&self, is_struct: bool) -> String {
        let mut is_nullable = self.is_nullable;
        let name = match &self.value_type {
            ValueType::Array(_) => self.list_type_decl(is_struct),
            _ => {
                // No-`Default` fields and `out` locals are `Option<T>`; AOSP `RustNameOf`.
                if !Self::can_be_defaulted(&self.value_type, is_struct)
                    && (is_struct || matches!(self.direction, Direction::Out))
                {
                    is_nullable = true;
                }
                self.type_decl(&self.value_type, true)
            }
        };

        if is_nullable && !name.starts_with(&format!("{OPTION}<")) {
            format!("{OPTION}<{name}>")
        } else {
            name
        }
    }

    /// True when a *struct field* of this type is stored as `Option<T>`
    /// only because the type has no `Default` (IBinder / interface `Strong`
    /// / `ParcelFileDescriptor`), not because it is `@nullable`. AOSP
    /// serializes such a non-nullable field by unwrapping it with
    /// `UNEXPECTED_NULL` on `None` instead of writing a null marker, so a
    /// real peer never sees an unexpected null. Mirrors the exact condition
    /// under which [`type_declaration`](Self::type_declaration) wraps the
    /// field in `Option` for `is_struct == true`.
    pub fn is_option_but_not_nullable(&self) -> bool {
        if self.is_nullable {
            return false;
        }
        match &self.value_type {
            // Array element nullability is handled separately.
            ValueType::Array(_) => false,
            vt => !Self::can_be_defaulted(vt, true),
        }
    }

    /// True when this arg is a non-nullable, out-only *variable* array of
    /// `ParcelFileDescriptor`. Such an array is represented as
    /// `Vec<Option<ParcelFileDescriptor>>` and filled with `None` placeholders
    /// by `resize_out_vec`, so before writing it back to the reply the server
    /// must reject any element the service left unset — a `None` has no valid
    /// fd wire encoding, so writing it would corrupt the reply for a
    /// conforming peer. Mirrors AOSP `generate_rust.cpp`'s
    /// `iter().any(Option::is_none)` → `UNEXPECTED_NULL` guard, which is
    /// deliberately scoped to out-only `ParcelFileDescriptor` arrays only
    /// (a null `IBinder` *is* a legal wire value, so binder arrays are not
    /// guarded; inout arrays are read in fully populated).
    pub fn out_array_needs_null_guard(&self) -> bool {
        matches!(self.direction, Direction::Out)
            && !self.is_nullable
            && self
                .array_types
                .first()
                .is_some_and(|sub| matches!(sub.value_type, ValueType::FileDescriptor))
    }

    /// How many `.flatten()` the null guard needs to reach the `Option`
    /// elements: one per nested fixed-size dimension beyond the first
    /// (`[[Option<_>; 3]; 2]` → 1). Zero when no guard applies.
    pub fn out_array_null_guard_flatten(&self) -> usize {
        if !self.out_array_needs_null_guard() {
            return 0;
        }
        self.array_types
            .first()
            .map_or(0, |sub| sub.sizes.len().saturating_sub(1))
    }

    /// True when this arg is a non-nullable, out-only *scalar* whose type has
    /// no `Default` and is therefore stored as `Option<T>`. The service may
    /// leave it unset, and `None` has no wire form the `.aidl` allows, so the
    /// server unwraps it into `UNEXPECTED_NULL` before writing the reply.
    /// Mirrors AOSP `generate_rust.cpp`'s `!arg->IsIn() && TypeNeedsOption(..)`
    /// → `.ok_or(binder::StatusCode::UNEXPECTED_NULL)?` arm.
    pub fn out_scalar_needs_unwrap(&self) -> bool {
        matches!(self.direction, Direction::Out)
            && !self.is_nullable
            && !matches!(self.value_type, ValueType::Array(_))
            && !Self::can_be_defaulted(&self.value_type, false)
    }

    fn func_list_type_decl_fixed(&self, array_info: &ArrayInfo) -> String {
        let fixed_array = self.make_fixed_array(array_info, false);

        match self.direction {
            Direction::Out | Direction::Inout => {
                if self.is_nullable {
                    format!("&mut {OPTION}<{fixed_array}>")
                } else {
                    format!("&mut {fixed_array}")
                }
            }
            _ => {
                if self.is_nullable {
                    format!("{OPTION}<&{fixed_array}>")
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
                    format!(
                        "&mut {OPTION}<{VEC}<{}>>",
                        Self::nullable_element(&sub_type.value_type, &type_name)
                    )
                } else if Self::can_be_defaulted(&sub_type.value_type, false)
                    || Self::is_primitive(&sub_type.value_type)
                {
                    // Enum is a primitive type.
                    format!("&mut {VEC}<{type_name}>")
                } else {
                    format!("&mut {VEC}<{OPTION}<{type_name}>>")
                }
            }
            Direction::Inout => {
                // Must match `list_type_decl`'s `Inout` arm: the server passes `&mut` its local.
                if self.is_nullable {
                    format!(
                        "&mut {OPTION}<{VEC}<{}>>",
                        Self::nullable_element(&sub_type.value_type, &type_name)
                    )
                } else {
                    format!("&mut {VEC}<{type_name}>")
                }
            }
            _ => {
                if self.is_nullable {
                    if Self::is_primitive(&sub_type.value_type) {
                        format!("{OPTION}<&[{type_name}]>")
                    } else {
                        format!("{OPTION}<&[{OPTION}<{type_name}>]>")
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
                        format!("{OPTION}<&str>")
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
                    let name = self.type_decl(&self.value_type, true);
                    // Mirrors `type_declaration`: a no-`Default` `out` arg is wrapped in `Option`.
                    if self.is_nullable
                        || (matches!(self.direction, Direction::Out)
                            && !Self::can_be_defaulted(&self.value_type, false))
                    {
                        format!("&mut {OPTION}<{name}>")
                    } else {
                        format!("&mut {name}")
                    }
                }
                _ => {
                    if Self::is_primitive(&self.value_type) {
                        self.type_decl(&self.value_type, true)
                    } else {
                        let name = self.type_decl(&self.value_type, true);
                        if self.is_nullable {
                            format!("{OPTION}<&{name}>")
                        } else {
                            format!("&{name}")
                        }
                    }
                }
            },
        })
    }

    pub fn const_type_decl(&self) -> Result<String, AidlError> {
        // String const arrays hold literals, which do not coerce to `String` in const position.
        if matches!(self.value_type, ValueType::Array(_)) {
            if let Some(info) = self.array_types.first() {
                // Must match `init_array_branch`'s predicate, which decides `Some(..)` elements.
                let element = |name: &str| {
                    if self.is_nullable && Self::is_aidl_nullable(&info.value_type) {
                        format!("{OPTION}<{name}>")
                    } else {
                        name.to_owned()
                    }
                };
                let outer = |name: String| {
                    if self.is_nullable {
                        format!("{OPTION}<{name}>")
                    } else {
                        name
                    }
                };
                let is_str = matches!(info.value_type, ValueType::String(_));
                if is_str && !info.is_fixed() {
                    return Ok(outer(format!("&[{}]", element("&str"))));
                }
                // Fixed-size consts are by value: `[1,2,3,]` does not coerce to a slice in const.
                if info.is_fixed() {
                    let base = if is_str {
                        element("&str")
                    } else {
                        element(&self.array_type_name(&info.value_type))
                    };
                    let name = info
                        .sizes
                        .iter()
                        .rev()
                        .fold(base, |acc, size| format!("[{acc}; {size}]"));
                    return Ok(outer(name));
                }
            }
        }
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

            if decl == STRING {
                format!("{}.as_str()", self.identifier)
            } else {
                match self.direction {
                    Direction::Inout | Direction::Out => {
                        format!("&mut {}", self.identifier)
                    }
                    _ => {
                        if decl.starts_with(&format!("{OPTION}<{VEC}<"))
                            || decl == format!("{OPTION}<{STRING}>")
                        {
                            format!("{}.as_deref()", self.identifier)
                        } else if decl.starts_with(&format!("{OPTION}<")) {
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

        // Absolute: `#[rsbinder::interface]` renders this under its parent's glob import.
        const OUT_DEFAULT: &str = "::core::default::Default::default()";
        let (mutable, init) = match self.direction {
            Direction::Out => (
                "mut ",
                self.fixed_array_init(OUT_DEFAULT, "::core::array::from_fn")
                    .unwrap_or_else(|| OUT_DEFAULT.to_owned()),
            ),
            Direction::Inout => ("mut ", format!("{reader}.read()?")),
            _ => ("", format!("{reader}.read()?")),
        };

        format!(
            "{mutable}{}: {} = {init}",
            self.identifier,
            self.type_declaration(false)
        )
    }

    /// `std::array::from_fn` init for a non-nullable array with a dim > 32 (no `Default` impl).
    fn fixed_array_default(&self) -> Option<String> {
        self.fixed_array_init(
            "::core::default::Default::default()",
            "::core::array::from_fn",
        )
    }

    /// `from_fn` once per dimension around `leaf`, spelled by the caller.
    fn fixed_array_init(&self, leaf: &str, from_fn: &str) -> Option<String> {
        if self.is_nullable {
            return None;
        }
        let info = self.array_types.first()?;
        if !info.is_fixed() || info.sizes.iter().all(|&n| n <= 32) {
            return None;
        }
        let mut init = leaf.to_string();
        for _ in 0..info.sizes.len() {
            init = format!("{from_fn}(|_| {init})");
        }
        Some(init)
    }

    pub fn default_value(&self) -> String {
        self.fixed_array_default()
            .unwrap_or_else(|| "::core::default::Default::default()".to_owned())
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

    fn validate_enum_value(
        &self,
        expr: &ConstExpr,
        target_lookup: &LookupDecl,
    ) -> Result<ConstExpr, AidlError> {
        let target_enum = target_lookup.ns.to_string(crate::Namespace::AIDL);
        // `ns` of a union's `Tag` is the union's; diagnostics name the `Tag` itself.
        let target_name = || match &target_lookup.decl {
            Declaration::Enum(e) if e.tag_of_union.is_some() => format!("{target_enum}.{}", e.name),
            _ => target_enum.clone(),
        };
        let calculated = expr
            .calculate()
            .map_err(|e| make_type_error(e.message, self.type_span))?;

        match &calculated.value {
            ValueType::Reference {
                enum_type,
                enum_name,
                member_name,
                ..
            } => {
                // Member names repeat across enums; the default must belong to the field's enum.
                if enum_type != &target_enum {
                    return Err(make_type_error(
                        format!(
                            "enum default value {}.{member_name} does not match target enum {}",
                            enum_aidl_name(enum_type, enum_name),
                            target_name(),
                        ),
                        self.type_span,
                    ));
                }

                Ok(calculated)
            }
            ValueType::Name(name) => Err(make_type_error(
                format!(
                    "unresolved enum default value {} for target enum {}",
                    name,
                    target_name()
                ),
                self.type_span,
            )),
            _ => Err(make_type_error(
                format!(
                    "enum default value {} is not a member of target enum {}",
                    calculated.to_value_string(),
                    target_name()
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
        let calculated = expr
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

        let enum_values = self.validate_enum_elements(&values, target_lookup)?;
        if let Some(info) = self.array_types.first() {
            self.check_fixed_arity(&enum_values, &info.sizes)?;
        }

        Ok(ValueType::Array(enum_values).to_init(
            param
                .with_fixed_array(is_fixed_array)
                .with_nullable(is_nullable),
        ))
    }

    // A nested array literal is one dimension of a multi-dimensional enum array.
    fn validate_enum_elements(
        &self,
        values: &[ConstExpr],
        target_lookup: &LookupDecl,
    ) -> Result<Vec<ConstExpr>, AidlError> {
        values
            .iter()
            .map(|value| match &value.value {
                ValueType::Array(inner) => Ok(ConstExpr::new(ValueType::Array(
                    self.validate_enum_elements(inner, target_lookup)?,
                ))),
                _ => self.validate_enum_value(value, target_lookup),
            })
            .collect()
    }

    /// Rank per dimension and exact count per fixed one, else the initializer would not compile.
    fn check_fixed_arity(&self, values: &[ConstExpr], sizes: &[i64]) -> Result<(), AidlError> {
        // `List<T>` carries no dims: it is one variable-length dimension.
        let sizes: &[i64] = if sizes.is_empty() { &[0] } else { sizes };
        let Some((&dim, rest)) = sizes.split_first() else {
            return Ok(());
        };
        // A variable-length dimension (0) skips only the count check.
        if dim > 0 && values.len() as i64 != dim {
            return Err(make_type_error(
                format!(
                    "fixed-size array default has {} element(s), expected {dim}",
                    values.len()
                ),
                self.type_span,
            ));
        }
        for v in values {
            match (&v.value, rest.is_empty()) {
                (ValueType::Array(inner), false) => self.check_fixed_arity(inner, rest)?,
                (ValueType::Array(_), true) | (_, false) => {
                    return Err(make_type_error(
                        format!(
                            "array default element {} has the wrong rank",
                            v.to_value_string()
                        ),
                        self.type_span,
                    ))
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn init_array_value(
        &self,
        expr: &ConstExpr,
        array_info: &ArrayInfo,
        param: InitParam,
        is_nullable: bool,
    ) -> Result<String, AidlError> {
        // Evaluation failure is a diagnostic (AOSP rejects), never `Default::default()`.
        let converted = expr
            .calculate()
            .and_then(|c| c.convert_to(&array_info.value_type))
            .map_err(|e| make_type_error(e.message, self.type_span))?;
        let ValueType::Array(values) = &converted.value else {
            return Err(make_type_error(
                "an array type requires an array literal default",
                self.type_span,
            ));
        };
        self.check_fixed_arity(values, &array_info.sizes)?;
        Ok(converted.value.to_init(
            param
                .with_fixed_array(array_info.is_fixed())
                .with_nullable(is_nullable),
        ))
    }

    /// Bare initializer for an array default; `init_value` adds the nullable `Some(..)`.
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
        self.init_array_value(expr, array_info, param, is_nullable)
    }

    /// Bare initializer for a scalar default; `init_value` adds `Some(..)` (never for enums).
    fn init_scalar_branch(&self, expr: &ConstExpr, param: InitParam) -> Result<String, AidlError> {
        if let Some(enum_lookup) = self.enum_lookup() {
            return self.init_enum_value(expr, &enum_lookup, param);
        }

        let scalar_param = param.with_fixed_array(false).with_nullable(false);
        // Every failure below is a diagnostic (AOSP rejects at build time), never a default.
        let calculated = expr
            .calculate()
            .map_err(|e| make_type_error(e.message, self.type_span))?;
        // Still a bare name: the reference does not resolve (typo or missing import).
        if let ValueType::Name(name) = &calculated.value {
            return Err(make_type_error(
                format!("cannot resolve constant reference '{name}'"),
                self.type_span,
            ));
        }
        // `convert_to` is element-wise, so an array literal would pass and emit a slice.
        if matches!(calculated.value, ValueType::Array(_)) {
            return Err(make_type_error(
                format!(
                    "an array literal cannot initialize the non-array type {}",
                    self.type_decl(&self.value_type, true)
                ),
                self.type_span,
            ));
        }
        Ok(match &calculated.value {
            // Enum targets returned above: a `UserDefined` target is a non-enum (AOSP rejects).
            ValueType::Reference { .. }
                if matches!(&self.value_type, ValueType::UserDefined(_)) =>
            {
                return Err(make_type_error(
                    format!(
                        "enum reference {} cannot initialize the non-enum type {}",
                        calculated.to_value_string(),
                        self.type_decl(&self.value_type, true)
                    ),
                    self.type_span,
                ));
            }
            ValueType::Reference { value, kind, .. } => {
                ConstExpr::new(ValueType::promoted_reference(*value, *kind))
                    .convert_to(&self.value_type)
                    .map_err(|e| make_type_error(e.message, self.type_span))?
                    .value
                    .to_init(scalar_param)
            }
            // Laxer than AOSP `ValueString` for char/float/double targets; non-enum types error.
            _ => calculated
                .convert_to(&self.value_type)
                .map_err(|e| make_type_error(e.message, self.type_span))?
                .value
                .to_init(scalar_param),
        })
    }

    pub(crate) fn init_value(
        &self,
        const_expr: Option<&ConstExpr>,
        param: InitParam,
    ) -> Result<String, AidlError> {
        let Some(expr) = const_expr else {
            // Struct-field path for dims > 32; `default_value`/`transaction_decl` cover the rest.
            if let Some(init) = self.fixed_array_default() {
                return Ok(init);
            }
            return Ok(ValueType::Void.to_init(param.with_fixed_array(false).with_nullable(false)));
        };

        let init_str = if matches!(self.value_type, ValueType::Array(_)) {
            self.init_array_branch(expr, param)?
        } else {
            self.init_scalar_branch(expr, param)?
        };

        Ok(if self.is_nullable {
            format!("::core::option::Option::Some({init_str})")
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

        assert_eq!(gen.type_declaration(false), "::std::string::String");

        let nullable_gen = gen.clone().nullable().unwrap();
        assert_eq!(
            nullable_gen.type_declaration(false),
            "::core::option::Option<::std::string::String>"
        );

        let array_gen = gen.array(&Vec::new()).unwrap();
        assert_eq!(
            array_gen.type_declaration(false),
            "::std::vec::Vec<::std::string::String>"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Out)
                .unwrap()
                .type_declaration(false),
            "::std::vec::Vec<::std::string::String>"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Inout)
                .unwrap()
                .type_declaration(false),
            "::std::vec::Vec<::std::string::String>"
        );

        let nullable_array_gen = array_gen.nullable().unwrap();
        assert_eq!(
            nullable_array_gen.type_declaration(false),
            "::core::option::Option<::std::vec::Vec<::core::option::Option<::std::string::String>>>"
        );
        assert_eq!(
            nullable_array_gen
                .clone()
                .direction(&Direction::Out)
                .unwrap()
                .type_declaration(false),
            "::core::option::Option<::std::vec::Vec<::core::option::Option<::std::string::String>>>"
        );
        assert_eq!(
            nullable_array_gen
                .direction(&Direction::Inout)
                .unwrap()
                .type_declaration(false),
            "::core::option::Option<::std::vec::Vec<::core::option::Option<::std::string::String>>>"
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
            "::core::option::Option<rsbinder::SIBinder>"
        );

        let array_gen = gen.array(&Vec::new()).unwrap();
        assert_eq!(
            array_gen.type_declaration(false),
            "::std::vec::Vec<rsbinder::SIBinder>"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Out)
                .unwrap()
                .type_declaration(false),
            "::std::vec::Vec<::core::option::Option<rsbinder::SIBinder>>"
        );
        // `inout` elements need no `Default`: AOSP `RustNameOf` keeps `element_mode = VALUE`.
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Inout)
                .unwrap()
                .type_declaration(false),
            "::std::vec::Vec<rsbinder::SIBinder>"
        );

        let nullable_array_gen = array_gen.nullable().unwrap();
        assert_eq!(
            nullable_array_gen.type_declaration(false),
            "::core::option::Option<::std::vec::Vec<::core::option::Option<rsbinder::SIBinder>>>"
        );
        assert_eq!(
            nullable_array_gen
                .clone()
                .direction(&Direction::Out)
                .unwrap()
                .type_declaration(false),
            "::core::option::Option<::std::vec::Vec<::core::option::Option<rsbinder::SIBinder>>>"
        );
        assert_eq!(
            nullable_array_gen
                .direction(&Direction::Inout)
                .unwrap()
                .type_declaration(false),
            "::core::option::Option<::std::vec::Vec<::core::option::Option<rsbinder::SIBinder>>>"
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
            "::core::option::Option<&rsbinder::ParcelFileDescriptor>"
        );

        let array_gen = gen.array(&Vec::new()).unwrap();
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
            "&mut ::std::vec::Vec<::core::option::Option<rsbinder::ParcelFileDescriptor>>"
        );
        // Must equal `list_type_decl(false)`: the server passes `&mut` its local here.
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Inout)
                .unwrap()
                .type_decl_for_func()
                .unwrap(),
            "&mut ::std::vec::Vec<rsbinder::ParcelFileDescriptor>"
        );

        let nullable_array_gen = array_gen.nullable().unwrap();
        assert_eq!(
            nullable_array_gen.type_decl_for_func().unwrap(),
            "::core::option::Option<&[::core::option::Option<rsbinder::ParcelFileDescriptor>]>"
        );
        assert_eq!(
            nullable_array_gen
                .clone()
                .direction(&Direction::Out)
                .unwrap()
                .type_decl_for_func()
                .unwrap(),
            "&mut ::core::option::Option<::std::vec::Vec<::core::option::Option<rsbinder::ParcelFileDescriptor>>>"
        );
        assert_eq!(
            nullable_array_gen
                .direction(&Direction::Inout)
                .unwrap()
                .type_decl_for_func()
                .unwrap(),
            "&mut ::core::option::Option<::std::vec::Vec<::core::option::Option<rsbinder::ParcelFileDescriptor>>>"
        );

        let gen = TypeGenerator::new(&NonArrayType {
            name: "boolean".to_owned(),
            generic: None,
            name_span: None,
        })
        .unwrap();
        let array_gen = gen.array(&Vec::new()).unwrap();
        assert_eq!(
            array_gen
                .direction(&Direction::Out)
                .unwrap()
                .type_decl_for_func()
                .unwrap(),
            "&mut ::std::vec::Vec<bool>"
        );

        // `ITestService.aidl` `ReverseUtf8CppStringList` input: `Option<&[Option<String>]>`.
        let gen = TypeGenerator::new(&NonArrayType {
            name: "String".to_owned(),
            generic: None,
            name_span: None,
        })
        .unwrap();
        let nullable_array_gen = gen.array(&Vec::new()).unwrap().nullable().unwrap();
        assert_eq!(
            nullable_array_gen.type_decl_for_func().unwrap(),
            "::core::option::Option<&[::core::option::Option<::std::string::String>]>"
        );
    }

    #[test]
    fn fixed_array_over_32_uses_array_from_fn_default() {
        // `int[40]` has no `Default`: `init_value(None)` and `default_value` need `from_fn`.
        let big = TypeGenerator::new(&NonArrayType {
            name: "int".to_owned(),
            generic: None,
            name_span: None,
        })
        .unwrap()
        .array(&[crate::parser::ArrayType {
            const_expr: Some(ConstExpr::new(ValueType::Int32(40))),
        }])
        .unwrap();

        let field_default = big
            .init_value(None, InitParam::builder().with_const(false))
            .unwrap();
        assert!(
            field_default.contains("::core::array::from_fn"),
            "parcelable field default must not be bare Default::default(): {field_default}"
        );
        assert!(big.default_value().contains("::core::array::from_fn"));

        // Every dimension <= 32 keeps `Default::default()`.
        let small = TypeGenerator::new(&NonArrayType {
            name: "int".to_owned(),
            generic: None,
            name_span: None,
        })
        .unwrap()
        .array(&[crate::parser::ArrayType {
            const_expr: Some(ConstExpr::new(ValueType::Int32(8))),
        }])
        .unwrap();
        assert_eq!(small.default_value(), "::core::default::Default::default()");
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

        let array_gen = gen.array(&Vec::new()).unwrap();
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
            .unwrap()
            .nullable()
            .unwrap();
        assert_eq!(
            array_nullable.type_declaration(true),
            "::core::option::Option<[bool; 2]>"
        );
    }
}
