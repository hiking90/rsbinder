// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::OnceLock;

use crate::const_expr::{ConstExpr, InitParam, ValueType};
use crate::parser::{self, *};

static CRATE_NAME: OnceLock<String> = OnceLock::new();

pub fn crate_name() -> String {
    CRATE_NAME.get_or_init(|| "rsbinder".to_owned()).clone()
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
                    t.const_expr
                        .clone()
                        .map_or_else(|| 0, |v| v.calculate().to_i64())
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
}

impl TypeGenerator {
    pub fn new(aidl_type: &NonArrayType) -> Self {
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
                    array_types.push(ArrayInfo::new_list(&gen.to_value_type(), &Vec::new()));
                    ValueType::Array(Vec::new())
                }
                None => panic!("Type \"List\" of AIDL must have Generic Type!"),
            },
            // "Map" => {
            //     is_primitive = false;
            //     is_map = true;
            //     ("HashMap".to_owned(), ValueType::Map())
            // }
            "FileDescriptor" => {
                panic!("FileDescriptor isn't supported by the AIDL compiler of rsbinder.");
            }
            "ParcelFileDescriptor" => ValueType::FileDescriptor,
            "ParcelableHolder" => ValueType::Holder,
            _ => ValueType::UserDefined(aidl_type.name.to_owned()),
        };

        Self {
            is_nullable: false,
            value_type,
            array_types,
            identifier: String::new(),
            direction: Default::default(),
        }
    }

    pub fn new_with_type(_type: &Type) -> Self {
        let mut this = Self::new(&_type.non_array_type);

        if !_type.array_types.is_empty() {
            this = this.array(&_type.array_types);
        }

        if check_annotation_list(&_type.annotation_list, AnnotationType::IsNullable).0 {
            this.nullable()
        } else {
            this
        }
    }

    fn is_aidl_nullable(value_type: &ValueType) -> bool {
        match value_type {
            ValueType::String(_)
            | ValueType::Array(_)
            | ValueType::FileDescriptor
            | ValueType::IBinder => true,
            ValueType::UserDefined(name) => {
                let lookup_decl = lookup_decl_from_name(name, crate::Namespace::AIDL);
                !matches!(lookup_decl.decl, Declaration::Enum(_))
            }
            _ => false,
        }
    }

    fn make_user_defined_type_name(&self, type_name: &str) -> String {
        let lookup_decl = lookup_decl_from_name(type_name, crate::Namespace::AIDL);
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
                let lookup_decl = lookup_decl_from_name(name, crate::Namespace::AIDL);
                matches!(lookup_decl.decl, Declaration::Enum(_))
            }
            ValueType::Reference { .. } => true,
            _ => value_type.is_primitive(),
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
                || matches!(
                    value_type,
                    ValueType::String(_)
                        | ValueType::Array(_)
                        | ValueType::Map(_, _)
                        | ValueType::Holder
                        | ValueType::UserDefined(_)
                )
        } else {
            Self::is_primitive(value_type)
                || match value_type {
                    ValueType::String(_)
                    | ValueType::Array(_)
                    | ValueType::Map(_, _)
                    | ValueType::Holder => true,
                    ValueType::UserDefined(name) => {
                        let lookup_decl = lookup_decl_from_name(name, crate::Namespace::AIDL);
                        matches!(
                            lookup_decl.decl,
                            Declaration::Enum(_)
                                | Declaration::Parcelable(_)
                                | Declaration::Union(_)
                        )
                    }
                    _ => false,
                }
        }
    }

    pub fn nullable(mut self) -> Self {
        if Self::is_primitive(&self.value_type) {
            panic!(
                "Primitive type({:?}) cannot get nullable annotation",
                self.value_type
            )
        }

        self.is_nullable = true;
        self
    }

    pub fn identifier(mut self, ident: &str) -> Self {
        self.identifier = format!("_arg_{ident}");
        self
    }

    pub fn direction(mut self, direction: &Direction) -> Self {
        if matches!(direction, Direction::Out | Direction::Inout)
            && (Self::is_primitive(&self.value_type)
                || matches!(self.value_type, ValueType::String(_)))
        {
            panic!("Primitive types and String can be an out or inout parameter.");
        }
        self.direction = direction.clone();
        self
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
            if self.is_nullable && Self::is_aidl_nullable(&array_info.value_type) {
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
        let fixed_array = self.make_fixed_array(array_info, is_struct);

        match self.direction {
            Direction::Out => {
                if self.is_nullable {
                    format!("Option<{fixed_array}>")
                } else {
                    fixed_array
                }
            }
            Direction::Inout => {
                if self.is_nullable {
                    format!("Option<{fixed_array}>")
                } else {
                    fixed_array
                }
            }
            _ => {
                if self.is_nullable {
                    format!("Option<{fixed_array}>")
                } else {
                    fixed_array
                }
            }
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
                } else if Self::can_be_defaulted(&sub_type.value_type, true) {
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
            Direction::Out => {
                if self.is_nullable {
                    format!("&mut Option<{fixed_array}>")
                } else {
                    format!("&mut {fixed_array}")
                }
            }
            Direction::Inout => {
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

    pub fn type_decl_for_func(&self) -> String {
        match &self.value_type {
            ValueType::Array(_) => self.func_list_type_decl(),
            ValueType::String(_) => match self.direction {
                Direction::Out | Direction::Inout => {
                    panic!("String cannot be an out or inout parameter.")
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
                        panic!("{:?} cannot be an out or inout parameter.", self.value_type);
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
        }
    }

    pub fn const_type_decl(&self) -> String {
        self.clone().direction(&Direction::In).type_decl_for_func()
    }

    fn check_identifier(&self) {
        if self.identifier.is_empty() {
            panic!("identifier is empty.");
        }
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
                let lookup_decl = lookup_decl_from_name(name, crate::Namespace::AIDL);
                match lookup_decl.decl {
                    Declaration::Enum(enum_decl) => {
                        let first = enum_decl.enumerator_list.first().unwrap();
                        format!(
                            "{}::{}",
                            self.make_user_defined_type_name(name),
                            first.identifier
                        )
                    }
                    _ => "Default::default()".to_owned(),
                }
            }
            _ => "Default::default()".to_owned(),
        }
    }

    pub(crate) fn init_value(&self, const_expr: Option<&ConstExpr>, param: InitParam) -> String {
        match const_expr {
            Some(expr) => {
                let init_str = if let ValueType::Array(_) = self.value_type {
                    let array_info = self.array_types.first().unwrap();
                    let is_nullable =
                        self.is_nullable && Self::is_aidl_nullable(&array_info.value_type);
                    expr.calculate()
                        .convert_to(&array_info.value_type)
                        .value
                        .to_init(
                            param
                                .with_fixed_array(array_info.is_fixed())
                                .with_nullable(is_nullable),
                        )
                } else {
                    let calculated = expr.calculate();
                    // Check if we have an enum reference and target is a primitive type
                    if let ValueType::Reference { value, .. } = &calculated.value {
                        if matches!(&self.value_type, ValueType::UserDefined(_)) {
                            // Target is an enum type, preserve enum reference
                            calculated
                                .value
                                .to_init(param.with_fixed_array(false).with_nullable(false))
                        } else {
                            // Target is a primitive type (e.g., int), use numeric value
                            ValueType::Int64(*value)
                                .to_init(param.with_fixed_array(false).with_nullable(false))
                        }
                    } else {
                        // Normal processing for non-enum references
                        calculated
                            .convert_to(&self.value_type)
                            .value
                            .to_init(param.with_fixed_array(false).with_nullable(false))
                    }
                };
                if self.is_nullable {
                    format!("Some({init_str})")
                } else {
                    init_str
                }
            }
            None => ValueType::Void.to_init(param.with_fixed_array(false).with_nullable(false)),
        }
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
        });

        assert_eq!(gen.type_declaration(false), "String");

        let nullable_gen = gen.clone().nullable();
        assert_eq!(nullable_gen.type_declaration(false), "Option<String>");

        let array_gen = gen.array(&Vec::new());
        assert_eq!(array_gen.type_declaration(false), "Vec<String>");
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Out)
                .type_declaration(false),
            "Vec<String>"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Inout)
                .type_declaration(false),
            "Vec<String>"
        );

        let nullable_array_gen = array_gen.nullable();
        assert_eq!(
            nullable_array_gen.type_declaration(false),
            "Option<Vec<Option<String>>>"
        );
        assert_eq!(
            nullable_array_gen
                .clone()
                .direction(&Direction::Out)
                .type_declaration(false),
            "Option<Vec<Option<String>>>"
        );
        assert_eq!(
            nullable_array_gen
                .direction(&Direction::Inout)
                .type_declaration(false),
            "Option<Vec<Option<String>>>"
        );
    }

    #[test]
    fn test_binder_declaration() {
        let gen = TypeGenerator::new(&NonArrayType {
            name: "IBinder".to_owned(),
            generic: None,
        });

        assert_eq!(gen.type_declaration(false), "rsbinder::SIBinder");

        let nullable_gen = gen.clone().nullable();
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
                .type_declaration(false),
            "Vec<Option<rsbinder::SIBinder>>"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Inout)
                .type_declaration(false),
            "Vec<Option<rsbinder::SIBinder>>"
        );

        let nullable_array_gen = array_gen.nullable();
        assert_eq!(
            nullable_array_gen.type_declaration(false),
            "Option<Vec<Option<rsbinder::SIBinder>>>"
        );
        assert_eq!(
            nullable_array_gen
                .clone()
                .direction(&Direction::Out)
                .type_declaration(false),
            "Option<Vec<Option<rsbinder::SIBinder>>>"
        );
        assert_eq!(
            nullable_array_gen
                .direction(&Direction::Inout)
                .type_declaration(false),
            "Option<Vec<Option<rsbinder::SIBinder>>>"
        );
    }

    #[test]
    fn test_type_decl_for_func() {
        let gen = TypeGenerator::new(&NonArrayType {
            name: "ParcelFileDescriptor".to_owned(),
            generic: None,
        });

        assert_eq!(gen.type_decl_for_func(), "&rsbinder::ParcelFileDescriptor");

        let nullable_gen = gen.clone().nullable();
        assert_eq!(
            nullable_gen.type_decl_for_func(),
            "Option<&rsbinder::ParcelFileDescriptor>"
        );

        let array_gen = gen.array(&Vec::new());
        assert_eq!(
            array_gen.type_decl_for_func(),
            "&[rsbinder::ParcelFileDescriptor]"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Out)
                .type_decl_for_func(),
            "&mut Vec<Option<rsbinder::ParcelFileDescriptor>>"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Inout)
                .type_decl_for_func(),
            "&mut Vec<rsbinder::ParcelFileDescriptor>"
        );

        let nullable_array_gen = array_gen.nullable();
        assert_eq!(
            nullable_array_gen.type_decl_for_func(),
            "Option<&[Option<rsbinder::ParcelFileDescriptor>]>"
        );
        assert_eq!(
            nullable_array_gen
                .clone()
                .direction(&Direction::Out)
                .type_decl_for_func(),
            "&mut Option<Vec<Option<rsbinder::ParcelFileDescriptor>>>"
        );
        assert_eq!(
            nullable_array_gen
                .direction(&Direction::Inout)
                .type_decl_for_func(),
            "&mut Option<Vec<Option<rsbinder::ParcelFileDescriptor>>>"
        );

        let gen = TypeGenerator::new(&NonArrayType {
            name: "boolean".to_owned(),
            generic: None,
        });
        let array_gen = gen.array(&Vec::new());
        assert_eq!(
            array_gen.direction(&Direction::Out).type_decl_for_func(),
            "&mut Vec<bool>"
        );

        // ITestService.aidl
        // fn ReverseUtf8CppStringList(&self, _arg_input: Option<&[Option<String>]>, _arg_repeated: &mut Option<Vec<Option<String>>>) -> binder::Result<Option<Vec<Option<String>>>>;
        let gen = TypeGenerator::new(&NonArrayType {
            name: "String".to_owned(),
            generic: None,
        });
        let nullable_array_gen = gen.array(&Vec::new()).nullable();
        assert_eq!(
            nullable_array_gen.type_decl_for_func(),
            "Option<&[Option<String>]>"
        );
    }

    #[test]
    fn test_func_call_param() {
        let gen = TypeGenerator::new(&NonArrayType {
            name: "String".to_owned(),
            generic: None,
        })
        .identifier("type");
        assert_eq!(gen.func_call_param(), "_arg_type.as_str()");
        assert_eq!(gen.nullable().func_call_param(), "_arg_type.as_deref()");

        let gen = TypeGenerator::new(&NonArrayType {
            name: "ParcelFileDescriptor".to_owned(),
            generic: None,
        })
        .identifier("type");
        assert_eq!(gen.func_call_param(), "&_arg_type");

        let array_gen = gen.array(&Vec::new());
        assert_eq!(
            array_gen.clone().nullable().func_call_param(),
            "_arg_type.as_deref()"
        );
        assert_eq!(
            array_gen
                .clone()
                .direction(&Direction::Out)
                .func_call_param(),
            "&mut _arg_type"
        );
        assert_eq!(
            array_gen.direction(&Direction::Inout).func_call_param(),
            "&mut _arg_type"
        );
    }

    #[test]
    fn test_type_decl_for_struct() {
        let gen = TypeGenerator::new(&NonArrayType {
            name: "boolean".to_owned(),
            generic: None,
        })
        .identifier("type");
        let array_nullable = gen
            .array(&[ArrayType {
                const_expr: Some(ConstExpr::new(ValueType::Byte(2))),
            }])
            .nullable();
        assert_eq!(array_nullable.type_declaration(true), "Option<[bool; 2]>");
    }
}
