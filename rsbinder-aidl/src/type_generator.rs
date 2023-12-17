use crate::parser::*;
use crate::const_expr::{ValueType, ConstExpr};

#[derive(Clone)]
pub struct TypeGenerator {
    is_nullable: bool,
    pub value_type: ValueType,
    array_types: Vec<ValueType>,
    pub identifier: String,
    direction: Direction,
}

impl TypeGenerator {
    pub fn new(aidl_type: &NonArrayType) -> Self {
        let mut array_types = Vec::new();
        let value_type = match aidl_type.name.as_str() {
            "boolean" => ValueType::Bool(false),
            "byte" => ValueType::Int8(0),
            "char" => ValueType::Char(Default::default()),
            "int" => ValueType::Int32(0),
            "long" => ValueType::Int64(0),
            "float" => ValueType::Float(0.),
            "double" => ValueType::Double(0.),
            "void" => ValueType::Void,
            "String" => ValueType::String(String::new()),
            "IBinder" => ValueType::IBinder,
            "List" => {
                match &aidl_type.generic {
                    Some(gen) => {
                        array_types.push(gen.to_value_type());
                        ValueType::Array(Vec::new())
                    }
                    None => panic!("Type \"List\" of AIDL must have Generic Type!"),
                }
            }
            // "Map" => {
            //     is_primitive = false;
            //     is_map = true;
            //     ("HashMap".to_owned(), ValueType::Map())
            // }
            "FileDescriptor" => {
                panic!("FileDescriptor isn't supported by the aidl generator of rsbinder.");
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
            this = this.array();
        }

        if check_annotation_list(&_type.annotation_list, AnnotationType::IsNullable).0 {
            this.nullable()
        } else {
            this
        }
    }

    fn make_user_defined_type_name(type_name: &str) -> String {
        let lookup_decl = lookup_decl_from_name(type_name, crate::Namespace::AIDL);
        let curr_ns = current_namespace();
        let ns = curr_ns.relative_mod(&lookup_decl.ns);
        let name = if !ns.is_empty() {
            format!("{}::{}", ns, lookup_decl.name.ns.last().unwrap())
        } else {
            let name = lookup_decl.name.ns.last().unwrap().to_owned();
            if curr_ns.ns.last().unwrap() == name.as_str() {
                format!("Box<{}>", name)    // To avoid, recursive type issue.
            } else {
                name
            }
        };

        match lookup_decl.decl {
            Declaration::Interface(_) => {
                format!("std::sync::Arc<dyn {}>", name)
            }
            _ => name,
        }
    }

    pub fn nullable(mut self) -> Self {
        if self.value_type.is_primitive() {
            panic!("Primitive type({:?}) cannot get nullable annotation", self.value_type)
        }

        self.is_nullable = true;
        self
    }

    pub fn identifier(mut self, ident: &str) -> Self {
        self.identifier = format!("_arg_{}", ident);
        self
    }

    pub fn direction(mut self, direction: &Direction) -> Self {
        if matches!(direction, Direction::Out | Direction::Inout) &&
            (self.value_type.is_primitive() || matches!(self.value_type, ValueType::String(_))) {
            panic!("Primitive types and String can be an out or inout parameter.");
        }
        self.direction = direction.clone();
        self
    }

    pub fn array(mut self) -> Self {
        match self.value_type {
            ValueType::Array(_) => self,
            _ => {
                self.array_types.push(self.value_type.clone());
                self.value_type = ValueType::Array(Vec::new());
                self
            }
        }
    }

    fn list_type_decl(&self) -> String {
        let sub_type = self.array_types.first().expect("array_types is empty.");
        match self.direction {
            Direction::Out => {
                format!("Vec<Option<{}>>", Self::type_decl(&self.value_type, Some(sub_type)))
            }
            Direction::Inout => {
                if self.is_nullable {
                    format!("Vec<Option<{}>>", Self::type_decl(&self.value_type, Some(sub_type)))
                } else {
                    format!("Vec<{}>", Self::type_decl(&self.value_type, Some(sub_type)))
                }
            }
            _ => {
                if self.is_nullable {
                    format!("Vec<Option<{}>>", Self::type_decl(&self.value_type, Some(sub_type)))
                } else {
                    format!("Vec<{}>", Self::type_decl(&self.value_type, Some(sub_type)))
                }
            }
        }
    }

    fn type_decl(value_type: &ValueType, sub_value: Option<&ValueType>) -> String {
        match value_type {
            ValueType::Void => "()".into(),
            ValueType::String(_) => "String".into(),
            ValueType::Int8(_) => "i8".into(),
            ValueType::Int32(_) => "i32".into(),
            ValueType::Int64(_) => "i64".into(),
            ValueType::Float(_) => "f32".into(),
            ValueType::Double(_) => "f64".into(),
            ValueType::Bool(_) => "bool".into(),
            ValueType::Char(_) => "u16".into(),
            ValueType::Array(_) => {
                // Vec<> is managed other functions. So, it just return sub_value type.
                Self::type_decl(sub_value.expect("Array must know the type of item."), None)
            }
            ValueType::IBinder => "rsbinder::StrongIBinder".into(),
            ValueType::FileDescriptor => "rsbinder::ParcelFileDescriptor".into(),
            ValueType::Holder => "rsbinder::ParcelableHolder".into(),
            ValueType::UserDefined(name) => Self::make_user_defined_type_name(name),
            _ => unreachable!(),
        }
    }

    pub fn type_declaration(&self) -> String {
        let name = match &self.value_type {
            ValueType::Array(_) => self.list_type_decl(),
            _ => Self::type_decl(&self.value_type, None),
        };

        if self.is_nullable {
            format!("Option<{name}>")
        } else {
            name
        }
    }

    fn func_list_type_decl(&self) -> String {
        let sub_type = self.array_types.first().expect("array_types is empty.");
        match self.direction {
            Direction::Out => {
                if self.is_nullable {
                    format!("&mut Option<Vec<Option<{}>>>", Self::type_decl(&self.value_type, Some(sub_type)))
                } else {
                    format!("&mut Vec<Option<{}>>", Self::type_decl(&self.value_type, Some(sub_type)))
                }
            }
            Direction::Inout => {
                if self.is_nullable {
                    format!("&mut Option<Vec<Option<{}>>>", Self::type_decl(&self.value_type, Some(sub_type)))
                } else {
                    format!("&mut Vec<{}>", Self::type_decl(&self.value_type, Some(sub_type)))
                }
            }
            _ => {
                if self.is_nullable {
                    format!("Option<&[Option<{}>]>", Self::type_decl(&self.value_type, Some(sub_type)))
                } else {
                    format!("&[{}]", Self::type_decl(&self.value_type, Some(sub_type)))
                }
            }
        }
    }

    pub fn type_decl_for_func(&self) -> String {
        if self.value_type.is_primitive() {
            Self::type_decl(&self.value_type, None)
        } else {
            match &self.value_type {
                ValueType::String(_) => {
                    if self.is_nullable {
                        "Option<&str>".into()
                    } else {
                        "&str".into()
                    }
                }
                ValueType::Array(_) => self.func_list_type_decl(),
                _  => {
                    let name = Self::type_decl(&self.value_type, None);
                    if self.is_nullable {
                        format!("Option<&{name}>")
                    } else {
                        format!("&{name}")
                    }
                }
            }
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

        if self.value_type.is_primitive() {
            self.identifier.clone()
        } else {
            let decl = self.type_declaration();

            if decl == "String" {
                format!("{}.as_str()", self.identifier)
            } else {
                match self.direction {
                    Direction::Inout | Direction::Out => {
                        format!("&mut {}", self.identifier)
                    }
                    _ => {
                        if decl.starts_with("Option<Vec<") {
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
            Direction::Out => {
                ("mut ", "Default::default()".to_owned())
            }
            Direction::Inout => {
                ("mut ", format!("{}.read()?", reader))
            }
            _ => {
                ("", format!("{}.read()?", reader))
            }
        };

        format!("{mutable}{}: {} = {init}", self.identifier, self.type_declaration())
    }

    pub fn init_value(&self, const_expr: Option<&ConstExpr>, is_const: bool) -> String {
        match const_expr {
            Some(expr) => {
                let expr = expr.calculate().convert_to(&self.value_type);
                expr.value.to_init(is_const)
            }
            None => ValueType::Void.to_init(is_const),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_type_declaration() {
        let gen = TypeGenerator::new(&NonArrayType{ name: "String".to_owned(), generic: None });

        assert_eq!(gen.type_declaration(), "String");

        let nullable_gen = gen.clone().nullable();
        assert_eq!(nullable_gen.type_declaration(), "Option<String>");

        let array_gen = gen.array();
        assert_eq!(array_gen.type_declaration(), "Vec<String>");
        assert_eq!(array_gen.clone().direction(&Direction::Out).type_declaration(), "Vec<Option<String>>");
        assert_eq!(array_gen.clone().direction(&Direction::Inout).type_declaration(), "Vec<String>");

        let nullable_array_gen = array_gen.nullable();
        assert_eq!(nullable_array_gen.type_declaration(), "Option<Vec<Option<String>>>");
        assert_eq!(nullable_array_gen.clone().direction(&Direction::Out).type_declaration(), "Option<Vec<Option<String>>>");
        assert_eq!(nullable_array_gen.direction(&Direction::Inout).type_declaration(), "Option<Vec<Option<String>>>");
    }

    #[test]
    fn test_type_decl_for_func() {
        let gen = TypeGenerator::new(&NonArrayType{ name: "IEmptyInterface".to_owned(), generic: None });

        assert_eq!(gen.type_decl_for_func(), "&IEmptyInterface");

        let nullable_gen = gen.clone().nullable();
        assert_eq!(nullable_gen.type_decl_for_func(), "Option<&IEmptyInterface>");

        let array_gen = gen.array();
        assert_eq!(array_gen.type_decl_for_func(), "&[IEmptyInterface]");
        assert_eq!(array_gen.clone().direction(&Direction::Out).type_decl_for_func(), "&mut Vec<Option<IEmptyInterface>>");
        assert_eq!(array_gen.clone().direction(&Direction::Inout).type_decl_for_func(), "&mut Vec<IEmptyInterface>");

        let nullable_array_gen = array_gen.nullable();
        assert_eq!(nullable_array_gen.type_decl_for_func(), "Option<&[Option<IEmptyInterface>]>");
        assert_eq!(nullable_array_gen.clone().direction(&Direction::Out).type_decl_for_func(), "&mut Option<Vec<Option<IEmptyInterface>>>");
        assert_eq!(nullable_array_gen.direction(&Direction::Inout).type_decl_for_func(), "&mut Option<Vec<Option<IEmptyInterface>>>");
    }

    #[test]
    fn test_func_call_param() {
        let gen = TypeGenerator::new(&NonArrayType{ name: "String".to_owned(), generic: None })
            .identifier("type");
        assert_eq!(gen.func_call_param(), "_arg_type.as_str()");
        assert_eq!(gen.nullable().func_call_param(), "_arg_type.as_ref()");

        let gen = TypeGenerator::new(&NonArrayType{ name: "IEmptyInterface".to_owned(), generic: None })
            .identifier("type");
        assert_eq!(gen.func_call_param(), "&_arg_type");

        let array_gen = gen.array();
        assert_eq!(array_gen.clone().nullable().func_call_param(), "_arg_type.as_deref()");
        assert_eq!(array_gen.clone().direction(&Direction::Out).func_call_param(), "&mut _arg_type");
        assert_eq!(array_gen.direction(&Direction::Inout).func_call_param(), "&mut _arg_type");
    }
}