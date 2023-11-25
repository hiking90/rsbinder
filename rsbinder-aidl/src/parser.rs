use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::panic;

use convert_case::{Case, Casing};

use pest::Parser;
#[derive(pest_derive::Parser)]
#[grammar = "aidl.pest"]
pub struct AIDLParser;

use crate::const_expr::{ConstExpr, ValueType};
use crate::{Namespace};

thread_local! {
    static DECLARATION_MAP: RefCell<HashMap<Namespace, Declaration>> = RefCell::new(HashMap::new());
    static NAMESPACE_STACK: RefCell<Vec<Namespace>> = RefCell::new(Vec::new());
    static DOCUMENT: RefCell<Document> = RefCell::new(Document::new());
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
        stack.borrow().last().map_or_else(|| panic!("There is no namespace in stack."),
            |namespace| namespace.clone())
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

pub fn lookup_decl_from_name(name: &str, style: &str) -> LookupDecl {
    let mut namespace = Namespace::new(name, style);

    let mut ns_vec = Vec::new();

    // 1, check if the type exists in the current namespace.
    let mut curr_ns = current_namespace();
    ns_vec.append(&mut make_ns_candidate(&curr_ns, &namespace));

    curr_ns.pop();  // For parent namespace
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

    // println!("namesapce: {:?}\nns_vec: {:?}\n", namespace, ns_vec);

    let (decl, ns) = DECLARATION_MAP.with(|hashmap| {
        for ns in &ns_vec {
            if let Some(decl) = hashmap.borrow().get(ns) {
                // println!("Found: {:?}\n", ns);
                return (decl.clone(), ns.clone());
            }
        }

        let curr_ns = current_namespace();
        if let Some(decl) = hashmap.borrow().get(&curr_ns) {
            // println!("Not Found: {:?}\n", curr_ns);
            return (decl.clone(), curr_ns)
        }

        panic!("Unknown namespace: {:?} for name: [{}]\n{:?}",
            ns_vec, name, hashmap.borrow().keys());
    });

    // leave max 2 items because the other items are for name space.
    if namespace.ns.len() > 2 {
        namespace.ns.drain(0..namespace.ns.len()-2);
    }

    LookupDecl {
        decl, ns, name: namespace
    }
}

fn make_const_expr(const_expr: Option<&ConstExpr>, lookup_decl: &LookupDecl) -> ConstExpr {
    if let Some(expr) = const_expr {
        expr.clone()
    } else {
        let ns = current_namespace().relative_mod(&lookup_decl.ns);

        let name = if !ns.is_empty() {
            format!("{}{}{}", ns, Namespace::RUST, lookup_decl.name.to_string(Namespace::RUST))
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
        },
        Declaration::Interface(ref decl) => {
            for var in &decl.constant_list {
                if var.identifier == lookup_ident {
                    return Some(make_const_expr(var.const_expr.as_ref(), lookup_decl));
                }
            }
            lookup_name_members(&decl.members, lookup_decl)
        }

        Declaration::Parcelable(ref decl) => {
            lookup_name_members(&decl.members, lookup_decl)
        }

        Declaration::Enum(ref decl) => {
            for enumerator in &decl.enumerator_list {
                if enumerator.identifier == lookup_ident {
                    return Some(make_const_expr(None, lookup_decl));
                }
            }
            lookup_name_members(&decl.members, lookup_decl)
        }

        Declaration::Union(ref decl) => {
            lookup_name_members(&decl.members, lookup_decl)
        }
    }
}

fn lookup_name_members(members: &Vec<Declaration>, lookup_decl: &LookupDecl) -> Option<ConstExpr> {
    for decl in members {
        if let Some(expr) = lookup_name_from_decl(decl, lookup_decl) {
            return Some(expr)
        }
    }
    None
}

// Normally, this function is used to generate Rust source code.
pub fn name_to_const_expr(name: &str) -> Option<ConstExpr> {
    let lookup_decl = lookup_decl_from_name(name, Namespace::AIDL);
    lookup_name_from_decl(&lookup_decl.decl, &lookup_decl)
}

#[derive(Debug)]
pub struct Interface {
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
    pub fn to_string(&self) -> String {
        let type_cast = self.r#type.type_cast();
        if self.constant {
            format!("pub const {}: {} = {};\n",
                self.const_identifier(),
                type_cast.const_type(), type_cast.init_type(self.const_expr.as_ref(), true))
        } else {
            format!("pub {}: {},\n", self.identifier(), type_cast.member_type())
        }
    }

    pub fn identifier(&self) -> String {
        self.identifier.to_owned()
    }

    pub fn const_identifier(&self) -> String {
        self.identifier.to_case(Case::UpperSnake)
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
    pub method_list: Vec<MethodDecl>,
    pub constant_list: Vec<VariableDecl>,
    pub members: Vec<Declaration>,
}

impl InterfaceDecl {
    pub fn pre_process(&mut self) {
        for decl in &mut self.constant_list {
            decl.const_expr = decl.const_expr.as_ref().map(|expr| expr.calculate());
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
                decl.const_expr = decl.const_expr.as_ref().map(|expr| expr.calculate());
            }
        }
    }
}

#[derive(Debug, Default, Clone)]
pub enum Direction {
    #[default] None,
    In,
    Out,
    Inout,
}

#[derive(Debug, Default, Clone)]
pub struct Arg {
    pub direction: Direction,
    pub r#type: Type,
    pub identifier: String,
}

impl Arg {
    pub fn to_string(&self, is_nullable: bool) -> (String, String, String) {
        // let param = format!("_arg_{}", self.identifier.to_case(Case::Snake));
        let param = format!("_arg_{}", self.identifier);
        let mut type_cast = self.r#type.type_cast();
        type_cast.set_fn_nullable(is_nullable);
        let def_arg = type_cast.fn_def_arg(&self.direction);
        let arg = format!("{}: {}",
            param.clone(), def_arg);
        (param, arg, def_arg)
    }

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
    pub arg_list: Vec<Arg>,
    pub intvalue: i64,
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
            _ => todo!(),
        }
    }

    pub fn set_namespace(&mut self, namespace: Namespace) {
        match self {
            Declaration::Parcelable(decl) => decl.namespace = namespace,
            Declaration::Interface(decl) => decl.namespace = namespace,
            Declaration::Enum(decl) => decl.namespace = namespace,
            Declaration::Union(decl) => decl.namespace = namespace,
            _ => todo!(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Declaration::Parcelable(decl) => &decl.name,
            Declaration::Interface(decl) => &decl.name,
            Declaration::Enum(decl) => &decl.name,
            Declaration::Union(decl) => &decl.name,
            _ => todo!(),
        }
    }

    pub fn members_mut(&mut self) -> &mut Vec<Declaration> {
        match self {
            Declaration::Parcelable(decl) => &mut decl.members,
            Declaration::Interface(decl) => &mut decl.members,
            Declaration::Enum(decl) => &mut decl.members,
            Declaration::Union(decl) => &mut decl.members,
            _ => todo!(),
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
    }
}

fn generic_type_args_to_string(args: &[Type]) -> String {
    let mut args_str = String::new();

    args.iter().for_each(|t| {
        let mut cast = t.type_cast();
        cast.set_generic(true);

        args_str.push_str(", ");
        args_str.push_str(&cast.member_type());
    });

    args_str[2..].into()
}

impl Generic {
    pub fn to_string(&self) -> String {
        match self {
            Generic::Type1 { type_args1, non_array_type, type_args2 } => {
                let cast = TypeCast::new(non_array_type);
                // cast.set_generic();
                format!("{}{}{}",
                    generic_type_args_to_string(type_args1),
                    cast.member_type(),
                    generic_type_args_to_string(type_args2))
            }
            Generic::Type2 { non_array_type, type_args } => {
                let cast = TypeCast::new(non_array_type);
                // cast.set_generic();
                format!("{}{}",
                    cast.member_type(),
                    generic_type_args_to_string(type_args))
            }
            Generic::Type3 { type_args } => {
                generic_type_args_to_string(type_args)
            }
        }
    }
}


#[derive(Debug, Default, Clone)]
pub struct NonArrayType {
    pub name: String,
    pub generic: Option<Box<Generic>>,
}

#[derive(Debug, Default, Clone)]
pub struct ArrayType {
    const_expr: Option<ConstExpr>,
}

impl ArrayType {
    pub fn to_string(&self) -> String {
        match &self.const_expr {
            Some(expr) => {
                let expr = expr.calculate();
                format!("[{}]", expr.to_string())
            }
            None => "".to_string(),
        }
    }

    pub fn is_vector(&self) -> bool {
        self.const_expr.is_none()
    }
}

#[derive(Debug, Default, Clone)]
pub struct Type {
    annotation_list: Vec<Annotation>,
    non_array_type: NonArrayType,
    array_types: Vec<ArrayType>,
}

impl Type {
    pub fn type_cast(&self) -> TypeCast {
        let mut cast = TypeCast::new(&self.non_array_type);

        cast.set_array_types(&self.array_types);
        cast.set_nullable(check_annotation_list(&self.annotation_list, AnnotationType::IsNullable).0);

        cast
    }
}

#[derive(Debug, Clone)]
pub struct TypeCast {
    pub aidl_type: NonArrayType,
    pub type_name: String,
    pub is_declared: bool,
    pub is_nullable: bool,
    pub is_fn_nullable: bool,
    pub is_vector: bool,
    pub is_generic: bool,
    pub value_type: ValueType,
}

impl TypeCast {
    fn new(aidl_type: &NonArrayType) -> TypeCast {
        let mut is_declared = false;
        let mut is_vector = false;
        let type_name = match aidl_type.name.as_str() {
            "boolean" => ("bool".to_owned(), ValueType::Bool(false)),
            "byte" => ("i8".to_owned(), ValueType::Int8(0)),
            "char" => ("u16".to_owned(), ValueType::Char(Default::default())),
            "int" => ("i32".to_owned(), ValueType::Int32(0)),
            "long" => ("i64".to_owned(), ValueType::Int64(0)),
            "float" => ("f32".to_owned(), ValueType::Float(0.)),
            "double" => ("f64".to_owned(), ValueType::Double(0.)),
            "void" => ("()".to_owned(), ValueType::Void),
            "String" => {
                ("String".to_owned(), ValueType::String(String::new()))
            }
            "IBinder" => {
                ("rsbinder::StrongIBinder".to_owned(), ValueType::IBinder)
            }
            "List" => {
                is_vector = true;
                match &aidl_type.generic {
                    Some(gen) => (gen.to_string(), ValueType::Array(Vec::new())),
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
            "ParcelFileDescriptor" => {
                ("rsbinder::ParcelFileDescriptor".to_owned(), ValueType::FileDescriptor)
            }
            "ParcelableHolder" => {
                ("rsbinder::ParcelableHolder".to_owned(), ValueType::Holder)
            }
            _ => {
                let lookup_decl = lookup_decl_from_name(aidl_type.name.as_str(), Namespace::AIDL);
                let curr_ns = current_namespace();
                let ns = curr_ns.relative_mod(&lookup_decl.ns);
                let type_name = if !ns.is_empty() {
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
                        is_declared = true;
                        // let type_name = format!("std::sync::Arc<dyn {}>", type_name);
                        (type_name.to_owned(), ValueType::UserDefined)
                    }
                    _ => (type_name.to_owned(), ValueType::UserDefined),
                }
            }
        };

        Self {
            aidl_type: aidl_type.clone(),
            type_name: type_name.0,
            value_type: type_name.1,
            is_declared,
            is_vector,
            is_nullable: false,
            is_generic: false,
            is_fn_nullable: false,
        }
    }

    fn format_option(is_ref: bool, name: &str) -> String {
        let name = if name.starts_with("Option<") {
            if name.chars().nth(7) == Some('&') {
                if is_ref {
                    &name[8..name.len() -1]
                } else {
                    &name[7..name.len() -1]
                }
            } else {
                &name[7..name.len() - 1]
            }
        } else {
            name
        };

        if is_ref {
            format!("Option<&{}>", name)
        } else {
            format!("Option<{}>", name)
        }
    }

    fn nullable_type(is_ref: bool, name: &str) -> String {
        if is_ref {
            format!("Option<&{}>", name)
        } else {
            format!("Option<{}>", name)
        }
    }

    fn general_type(is_ref: bool, name: &str) -> String {
        if is_ref {
            format!("&{}", name)
        } else {
            name.to_owned()
        }
    }

    pub fn type_name(&self, dir: &Direction) -> String {
        let mut is_option = false;
        let type_name = if self.is_declared {
            if self.is_nullable {
                is_option = true;
            }
            format!("std::sync::Arc<dyn {}>", self.type_name)
        } else {
            match self.value_type {
                ValueType::FileDescriptor | ValueType::Holder |
                ValueType::IBinder => {
                    if (!self.is_generic && !self.is_vector) && self.is_nullable {
                        is_option = true;
                    }
                },
                _ => {
                    if self.is_nullable {
                        is_option = true;
                    }
                }
            }
            self.type_name.to_owned()
        };

        match dir {
            Direction::In => {
                if self.is_vector {
                    if is_option {
                        if self.is_nullable {
                            format!("Option<&[{}]>", Self::format_option(false, &type_name))
                        } else {
                            format!("&[{}]", Self::format_option(false, &type_name))
                        }
                    } else {
                        format!("&[{}]", type_name)
                    }
                } else {
                    let type_name = match self.value_type {
                        ValueType::String(_) => "str".to_owned(),
                        ValueType::Bool(_) | ValueType::Int8(_) | ValueType::Int32(_) | ValueType::Int64(_) |
                        ValueType::Char(_) | ValueType::Float(_) | ValueType::Double(_) => {
                            return type_name;
                        }
                        _ => type_name,
                    };
                    let type_name = if let ValueType::String(_) = self.value_type {
                        "str".to_owned()
                    } else {
                        type_name
                    };
                    if is_option {
                        Self::format_option(true, &type_name)
                    } else {
                        format!("&{}", type_name)
                    }
                }
            }
            Direction::Out => {
                if self.is_fn_nullable {
                    is_option = true;
                }
                let type_name = self.type_name(&Direction::None);
                // if type_name.starts_with("Option<") {
                //     is_option = false;
                // }
                if is_option {
                    format!("&mut {}", Self::format_option(false, &type_name))
                } else {
                    format!("&mut {}", type_name)
                }
            }
            Direction::Inout => {
                let mut type_cast = self.clone();
                type_cast.set_fn_nullable(false);
                type_cast.type_name(&Direction::Out)
            }
            Direction::None => {
                let type_name = if self.is_vector {
                    if self.is_nullable {
                        format!("Vec<{}>", Self::format_option(false, &type_name))
                    } else {
                        format!("Vec<{}>", type_name)
                    }
                } else {
                    type_name
                };

                if is_option {
                    Self::format_option(false, &type_name)
                } else {
                    type_name
                }
            }
        }
    }

    pub fn fn_def_arg(&self, direction: &Direction) -> String {
        let direction = if let Direction::None = direction {
            Direction::In
        } else {
            direction.clone()
        };

        self.type_name(&direction)
    }

    fn set_nullable(&mut self, is_nullable: bool) {
        self.is_nullable = is_nullable;
    }

    pub fn set_fn_nullable(&mut self, is_nullable: bool) {
        self.is_fn_nullable = is_nullable;
    }

    fn set_generic(&mut self, is_generic: bool) {
        self.is_generic = is_generic;
    }

    fn set_array_types(&mut self, array_types: &[ArrayType]) {
        // TODO: implement Vector.....
        // &Vec<T>
        array_types.iter().for_each(|t| {
            if t.is_vector() {
                self.is_vector = true;
            } else {
                self.type_name = format!("{}[{}]", self.type_name, t.to_string());
            }
        });
    }

    pub fn value_type(&self) -> ValueType {
        self.value_type.clone()
    }

    pub fn const_type(&self) -> String {
        self.type_name(&Direction::In)
    }

    pub fn member_type(&self) -> String {
        let type_name = self.type_name(&Direction::None);
        if self.is_declared {
            Self::format_option(false, &type_name)
        } else {
            type_name
        }
    }

    pub fn init_type(&self, const_expr: Option<&ConstExpr>, is_const: bool) -> String {
        match const_expr {
            Some(expr) => {
                let expr = expr.calculate().convert_to(&self.value_type());
                expr.value.to_init(is_const)
            }
            None => ValueType::Void.to_init(is_const),
        }
    }

    pub fn return_type(&self) -> String {
        if let ValueType::Void = self.value_type {
            self.type_name.to_owned()
        } else {
            let type_name = self.type_name(&Direction::None);
            if self.is_fn_nullable {
                Self::format_option(false, &type_name)
            } else {
                type_name
            }
        }
    }
}

#[derive(PartialEq)]
pub enum AnnotationType {
    IsNullable,
    JavaOnly,
    RustDerive,
}

pub fn check_annotation_list(annotation_list: &Vec<Annotation>, query_type: AnnotationType) -> (bool, String) {
    for annotation in annotation_list {
        match query_type {
            AnnotationType::IsNullable if annotation.annotation == "@nullable" => return (true, "".to_owned()),
            AnnotationType::JavaOnly if annotation.annotation.starts_with("@JavaOnly") => return (true, "".to_owned()),
            AnnotationType::RustDerive if annotation.annotation == "@RustDerive" => {
                let mut derives = Vec::new();

                for param in &annotation.parameter_list {
                    if param.const_expr.to_bool() {
                        derives.push(param.identifier.to_owned())
                    }
                }

                return (true, derives.join(","))
            }
            _ => {}
        }
    }

    (false, "".to_owned())
}

pub fn get_backing_type(annotation_list: &Vec<Annotation>) -> TypeCast {
    // parse "@Backing(type="byte")"
    for annotation in annotation_list {
        if annotation.annotation == "@Backing" {
            for param in &annotation.parameter_list {
                if param.identifier == "type" {
                    return TypeCast::new(&NonArrayType {
                    // The cstr is enclosed in quotes.
                        name: param.const_expr.to_string().trim_matches('"').into(),
                        generic: None,
                    });
                }
            }
        }
    }

    TypeCast::new(&NonArrayType {
    // The cstr is enclosed in quotes.
        name: "byte".into(),
        generic: None,
    })
}

fn parse_unary(mut pairs: pest::iterators::Pairs<Rule>) -> ConstExpr {
    let operator = pairs.next().unwrap().as_str().to_owned();
    let factor = parse_factor(pairs.next().unwrap().into_inner().next().unwrap());
    ConstExpr::new_unary(&operator, factor)
}

fn parse_intvalue(arg_value: &str) -> ConstExpr {
    let mut is_u8 = false;
    let mut is_long = false;

    let (value, radix) = if arg_value.starts_with("0x") || arg_value.starts_with("0X") {
        (&arg_value[2..], 16)
    } else {
        (arg_value, 10)
    };

    let value = if value.ends_with('l') || value.ends_with('L') {
        is_long = true;
        &value[..value.len() -1]
    } else if let Some(stripped) = value.strip_suffix("u8") {
        is_u8 = true;
        stripped
    } else {
        value
    };

    if radix == 16 {
        if is_u8 {
            let parsed_value = u8::from_str_radix(value, radix).map_err(|err| {
                    eprintln!("{:?}\nparse_intvalue() - Invalid u8 value: {}, radix: {}\n", err, arg_value, radix);
                    err
                }).unwrap();
            ConstExpr::new(ValueType::Int8(parsed_value as i8 as _))
        } else if !is_long {
            if let Ok(parsed_value) = u32::from_str_radix(value, radix) {
                ConstExpr::new(ValueType::Int32(parsed_value as i32 as _))
            } else {
                let parsed_value = u64::from_str_radix(value, radix).map_err(|err| {
                        eprintln!("{:?}\nparse_intvalue() - Invalid u64 value: {}, radix: {}\n", err, arg_value, radix);
                        err
                    }).unwrap();
                ConstExpr::new(ValueType::Int64(parsed_value as i64 as _))
            }
        } else {
            let parsed_value = u64::from_str_radix(value, radix).map_err(|err| {
                    eprintln!("{:?}\nparse_intvalue() - Invalid u64 value: {}, radix: {}\n", err, arg_value, radix);
                    err
                }).unwrap();
            ConstExpr::new(ValueType::Int64(parsed_value as i64 as _))
        }
    } else {
        let parsed_value = i64::from_str_radix(value, radix).map_err(|err| {
                eprintln!("{:?}\nparse_intvalue() - Invalid int value: {}, radix: {}\n", err, arg_value, radix);
                err
            }).unwrap();
        if is_u8 {
            if parsed_value > u8::MAX.into() || parsed_value < 0 {
                panic!("u8 is overflowed. {}", parsed_value);
            }
            ConstExpr::new(ValueType::Int8(parsed_value as i8 as _))
        } else if is_long {
            ConstExpr::new(ValueType::Int64(parsed_value as _))
        } else {
            if parsed_value <= i8::MAX.into() && parsed_value >= i8::MIN.into() {
                ConstExpr::new(ValueType::Int8(parsed_value as i8 as _))
            } else if parsed_value <= i32::MAX.into() && parsed_value >= i32::MIN.into() {
                ConstExpr::new(ValueType::Int32(parsed_value as i32 as _))
            } else {
                ConstExpr::new(ValueType::Int64(parsed_value as _))
            }
        }
    }
}

fn parse_value(pair: pest::iterators::Pair<Rule>) -> ConstExpr {
    match pair.as_rule() {

        // Rule::const_expr => { parse_const_expr(pair.into_inner()) }
        Rule::qualified_name => { ConstExpr::new(ValueType::Name(pair.as_str().into())) }
        // Rule::C_STR => { ConstExpr::CStr(pair.as_str().into()) }
        Rule::HEXVALUE => { parse_intvalue(pair.as_str()) }
        Rule::FLOATVALUE => {
            let value = pair.as_str();
            let value = if let Some(stripped) = value.strip_suffix('f') {
                stripped
            } else {
                value
            };
            ConstExpr::new(ValueType::Double(value.parse::<f64>().unwrap() as _))
        }
        Rule::INTVALUE => { parse_intvalue(pair.as_str()) }
        Rule::TRUE_LITERAL => { ConstExpr::new(ValueType::Bool(true)) }
        Rule::FALSE_LITERAL => { ConstExpr::new(ValueType::Bool(false)) }
        _ => unreachable!("Unexpected rule in parse_value(): {}", pair),
    }
}

fn parse_factor(pair: pest::iterators::Pair<Rule>) -> ConstExpr {
    // println!("parse_factor {:?}", pair);
    match pair.as_rule() {
        Rule::expression => {
            parse_expression(pair.clone().into_inner())
        }
        Rule::unary => {
            parse_unary(pair.into_inner())
        }
        Rule::value => {
            parse_value(pair.into_inner().next().unwrap())
        }
        _ => unreachable!("Unexpected rule in parse_factor(): {}", pair),
    }
}

fn parse_expression_term(pair: pest::iterators::Pair<Rule>) -> ConstExpr {
    match pair.as_rule() {
        Rule::equality | Rule::comparison |
        Rule::bitwise_or | Rule::bitwise_xor | Rule::bitwise_and | Rule::shift | Rule::arith |
        Rule::logical_or | Rule::logical_and => {
            parse_expression(pair.clone().into_inner())
        }
        Rule::factor => {
            parse_factor(pair.into_inner().next().unwrap())
        }
        _ => unreachable!("Unexpected rule in Rule::parse_expression_into: {}", pair),
    }
}

fn parse_expression(mut pairs: pest::iterators::Pairs<Rule>) -> ConstExpr {
    let mut lhs = parse_expression_term(pairs.next().unwrap());

    while let Some(pair) = pairs.next() {
        let op = pair.as_str().to_owned();
        let rhs = parse_expression_term(pairs.next().unwrap());

        lhs = ConstExpr::new_expr(lhs, &op, rhs)
    }

    lhs
}

fn parse_string_term(pair: pest::iterators::Pair<Rule>) -> ConstExpr {
    match pair.as_rule() {
        Rule::C_STR => {
            let string = pair.as_str();
            ConstExpr::new(ValueType::String(string[1 .. string.len()-1].into()))
        }
        Rule::qualified_name => { ConstExpr::new(ValueType::Name(pair.as_str().into())) }
        _ => unreachable!("Unexpected rule in Rule::parse_string_term: {}", pair),
    }
}

fn parse_string_expr(pairs: pest::iterators::Pairs<Rule>) -> ConstExpr {
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

    expr.expect("Parsing error in String expression.")
}

fn parse_const_expr(pair: pest::iterators::Pair<Rule>) -> ConstExpr {
    match pair.as_rule() {
        Rule::constant_value_list => {
            let mut value_list = Vec::new();
            for pair in pair.into_inner() {
                match pair.as_rule() {
                    Rule::const_expr => {
                        value_list.push(parse_const_expr(pair.into_inner().next().unwrap()));
                    }
                    _ => unreachable!("Unexpected rule in Rule::constant_value_list: {}", pair),
                }
            }
            ConstExpr::new(ValueType::Array(value_list))
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
                        return ConstExpr::new(ValueType::Char(ch));
                    }
                }
            }
            unreachable!()
        }

        Rule::expression => {
            parse_expression(pair.clone().into_inner())
        }

        Rule::string_expr => {
            parse_string_expr(pair.into_inner())
        }

        _ => unreachable!("Unexpected rule in parse_const_expr(): {}", pair),
    }
}

fn parse_parameter(pairs: pest::iterators::Pairs<Rule>) -> Parameter {
    let mut parameter = Parameter {
        identifier: "".to_string(),
        const_expr: ConstExpr::default(),
    };

    for pair in pairs {
        match pair.as_rule() {
            Rule::identifier => { parameter.identifier = pair.as_str().into(); }
            Rule::const_expr => {
                parameter.const_expr = parse_const_expr(pair.into_inner().next().unwrap());
            }
            _ => unreachable!("Unexpected rule in parse_parameter(): {}", pair),
        }
    }

    parameter
}

fn parse_parameter_list(pairs: pest::iterators::Pairs<Rule>) -> Vec<Parameter> {
    let mut list = Vec::new();
    for pair in pairs {
        list.push(parse_parameter(pair.into_inner()));
    }

    list
}

fn parse_annotation(pairs: pest::iterators::Pairs<Rule>) -> Annotation {
    let mut annotation = Annotation::default();
    for pair in pairs {
        match pair.as_rule() {
            Rule::ANNOTATION => {
                annotation.annotation = pair.as_str().into();
            }

            Rule::const_expr => {
                annotation.const_expr = Some(parse_const_expr(pair.into_inner().next().unwrap()));
            }

            Rule::parameter_list => {
                annotation.parameter_list = parse_parameter_list(pair.into_inner());
            }

            _ => unreachable!("Unexpected rule in parse_annotation(): {}", pair),
        }
    }

    annotation
}

fn parse_annotation_list(pairs: pest::iterators::Pairs<Rule>) -> Vec<Annotation> {
    let mut annotation_list = Vec::new();
    for pair in pairs {
        annotation_list.push(parse_annotation(pair.into_inner()));
    }

    annotation_list
}

fn parse_type_args(pairs: pest::iterators::Pairs<Rule>) -> Vec<Type> {
    let mut res = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::r#type => { res.push(parse_type(pair.into_inner())) }
            _ => unreachable!("Unexpected rule in parse_type_args(): {}", pair),
        }
    }

    res
}

fn parse_non_array_type(pairs: pest::iterators::Pairs<Rule>) -> NonArrayType {
    let mut non_array_type = NonArrayType::default();

    for pair in pairs {
        match pair.as_rule() {
            // Rule::annotation_list => { non_array_type.annotation_list = parse_annotation_list(pair.into_inner()); }
            Rule::qualified_name => { non_array_type.name = pair.as_str().into(); }
            Rule::generic_type1 => {
                let mut pairs = pair.into_inner();
                let generic = Generic::Type1 {
                    type_args1: parse_type_args(pairs.next().unwrap().into_inner()),
                    non_array_type: parse_non_array_type(pairs.next().unwrap().into_inner()),
                    type_args2: parse_type_args(pairs.next().unwrap().into_inner()),
                };

                non_array_type.generic = Some(Box::new(generic));
            }

            Rule::generic_type2 => {
                let mut pairs = pair.into_inner();
                let generic = Generic::Type2 {
                    non_array_type: parse_non_array_type(pairs.next().unwrap().into_inner()),
                    type_args: parse_type_args(pairs.next().unwrap().into_inner()),
                };

                non_array_type.generic = Some(Box::new(generic));
            }
            Rule::generic_type3 => {
                let mut pairs = pair.into_inner();
                let generic = Generic::Type3 {
                    type_args: parse_type_args(pairs.next().unwrap().into_inner()),
                };

                non_array_type.generic = Some(Box::new(generic));
            }
            _ => { todo!(); }
        }
    }

    non_array_type
}

fn parse_array_type(pairs: pest::iterators::Pairs<Rule>) -> ArrayType {
    let mut array_type = ArrayType::default();

    for pair in pairs {
        match pair.as_rule() {
            // Rule::annotation_list => { array_type.annotation_list = parse_annotation_list(pair.into_inner()); }
            Rule::const_expr => {
                array_type.const_expr = Some(parse_const_expr(pair.into_inner().next().unwrap()));
            }
            _ => unreachable!("Unexpected rule in parse_array_type(): {}", pair),
        }
    }

    array_type
}

fn parse_type(pairs: pest::iterators::Pairs<Rule>) -> Type {
    let mut r#type = Type::default();

    for pair in pairs {
        match pair.as_rule() {
            Rule::annotation_list => { r#type.annotation_list = parse_annotation_list(pair.into_inner()); }
            Rule::non_array_type => { r#type.non_array_type = parse_non_array_type(pair.into_inner()); }
            Rule::array_type => { r#type.array_types.push(parse_array_type(pair.into_inner())); }
            _ => { todo!(); }
        }
    }

    r#type
}

fn parse_variable_decl(pairs: pest::iterators::Pairs<Rule>, constant: bool) -> VariableDecl {
    let mut decl = VariableDecl { constant, .. Default::default() };

    for pair in pairs {
        match pair.as_rule() {
            Rule::annotation_list => { decl.annotation_list = parse_annotation_list(pair.into_inner()); }
            Rule::r#type => { decl.r#type = parse_type(pair.into_inner()); }
            Rule::identifier => { decl.identifier = pair.as_str().into(); }
            Rule::const_expr => {
                match pair.into_inner().next() {
                    Some(pair) => decl.const_expr = Some(parse_const_expr(pair)),
                    None => decl.const_expr = None,
                }
            }
            _ => unreachable!("Unexpected rule in parse_variable_decl(): {}\t{}", pair, pair.as_str()),
        }
    }

    decl
}

fn parse_arg(pairs: pest::iterators::Pairs<Rule>) -> Arg {
    let mut arg = Arg::default();

    for pair in pairs {
        match pair.as_rule() {
            Rule::direction => {
                arg.direction = match pair.as_str() {
                    "in" => Direction::In,
                    "out" => Direction::Out,
                    "inout" => Direction::Inout,
                    _ => panic!("Unsupported direction: {}", pair.as_str()),
                };
            }
            Rule::r#type => { arg.r#type = parse_type(pair.into_inner()); }
            Rule::identifier => { arg.identifier = pair.as_str().into(); }
            _ => unreachable!("Unexpected rule in parse_arg(): {}", pair),
        }
    }

    arg
}

fn parse_method_decl(pairs: pest::iterators::Pairs<Rule>) -> MethodDecl {
    let mut decl = MethodDecl::default();

    for pair in pairs {
        match pair.as_rule() {
            Rule::annotation_list => { decl.annotation_list = parse_annotation_list(pair.into_inner()); }
            Rule::ONEWAY => { decl.oneway = true; }
            Rule::r#type => { decl.r#type = parse_type(pair.into_inner()); }
            Rule::identifier => { decl.identifier = pair.as_str().into(); }
            Rule::arg_list => {
                for pair in pair.into_inner() {
                    match pair.as_rule() {
                        Rule::arg => { decl.arg_list.push(parse_arg(pair.into_inner())); }
                        _ => unreachable!("Unexpected rule in parse_method_decl(): {}, \"{}\"", pair, pair.as_str()),
                    }
                }
            }
            Rule::INTVALUE => {
                let expr = parse_intvalue(pair.as_str()). calculate();
                decl.intvalue = match expr.value {
                    ValueType::Int8(v) => v as _,
                    ValueType::Int32(v) => v as _,
                    ValueType::Int64(v) => v,
                    _ => unreachable!("Unexpected Expression in parse_method_decl(): {}, \"{}\"", pair, pair.as_str()),
                };
            }
            _ => unreachable!("Unexpected rule in parse_method_decl(): {}, \"{}\"", pair, pair.as_str()),

        }
    }

    decl
}

fn parse_interface_members(pairs: pest::iterators::Pairs<Rule>, interface: &mut InterfaceDecl) {

    for pair in pairs {
        match pair.as_rule() {
            Rule::method_decl => {
                 interface.method_list.push(parse_method_decl(pair.into_inner()));
            }

            Rule::constant_decl => {
                interface.constant_list.push(parse_variable_decl(pair.into_inner(), true));
            }

            Rule::interface_members => {
                parse_interface_members(pair.into_inner(), interface);
            }

            Rule::decl => {
                interface.members.append(&mut parse_decl(pair.into_inner()));
            }

            _ => unreachable!("Unexpected rule in parse_interface_members(): {}", pair),
        }
    }
}

fn parse_interface_decl(annotation_list: Vec<Annotation>, pairs: pest::iterators::Pairs<Rule>) -> Declaration {
    let mut interface = InterfaceDecl { annotation_list, .. Default::default() };

    for pair in pairs {
        match pair.as_rule() {
            Rule::ONEWAY => { interface.oneway = true; }

            Rule::qualified_name => { interface.name = pair.as_str().into(); }

            Rule::interface_members => { parse_interface_members(pair.into_inner(), &mut interface); }

            _ => unreachable!("Unexpected rule in parse_interface_decl(): {}", pair),
        }
    }

    Declaration::Interface(interface)
}

fn parse_parcelable_members(pairs: pest::iterators::Pairs<Rule>) -> Vec<Declaration> {
    let mut res = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::variable_decl => {
                res.push(Declaration::Variable(parse_variable_decl(pair.into_inner(), false)));
            }
            Rule::constant_decl => {
                res.push(Declaration::Variable(parse_variable_decl(pair.into_inner(), true)));
            }
            Rule::decl => { res.append(&mut parse_decl(pair.into_inner())) }
            _ => unreachable!("Unexpected rule in parse_parcelable_members(): {}", pair),
        }
    }

    res
}

fn parse_optional_type_params(pairs: pest::iterators::Pairs<Rule>) -> Vec<String> {
    let mut res = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::identifier => { res.push(pair.as_str().into()) }
            _ => unreachable!("Unexpected rule in parse_optional_type_params(): {}", pair),
        }
    }

    res
}

fn parse_parcelable_decl(annotation_list: Vec<Annotation>, pairs: pest::iterators::Pairs<Rule>) -> Declaration {
    let mut parcelable = ParcelableDecl{ annotation_list, .. Default::default() };

    for pair in pairs {
        match pair.as_rule() {
            Rule::qualified_name => { parcelable.name = pair.as_str().into(); }

            Rule::optional_type_params => {
                parcelable.type_params = parse_optional_type_params(pair.into_inner());
            }

            Rule::parcelable_members => {
                parcelable.members.append(&mut parse_parcelable_members(pair.into_inner()));
            }

            Rule::C_STR => { parcelable.cpp_header = pair.as_str().into(); }

            _ => unreachable!("Unexpected rule in parse_parcelable_decl(): {}", pair),
        }
    }

    Declaration::Parcelable(parcelable)
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
    pub enumerator_list: Vec<Enumerator>,
    pub members: Vec<Declaration>,
}

fn parse_enumerator(pairs: pest::iterators::Pairs<Rule>) -> Enumerator {
    let mut res = Enumerator::default();

    for pair in pairs {
        match pair.as_rule() {
            Rule::identifier => { res.identifier = pair.as_str().into(); }
            Rule::const_expr => {
                res.const_expr = Some(parse_const_expr(pair.into_inner().next().unwrap()));
            }
            _ => unreachable!("Unexpected rule in parse_enumerator(): {}", pair),
        }
    }

    res
}

fn parse_enum_decl(annotation_list: Vec<Annotation>, pairs: pest::iterators::Pairs<Rule>) -> Declaration {
    let mut enum_decl = EnumDecl { annotation_list: annotation_list.clone(), .. Default::default() };

    for pair in pairs {
        match pair.as_rule() {
            Rule::qualified_name => {
                enum_decl.name = pair.as_str().into();
            }
            Rule::enumerator => {
                enum_decl.enumerator_list.push(parse_enumerator(pair.into_inner()))
            }
            _ => unreachable!("Unexpected rule in parse_enum_decl(): {}", pair),
        }
    }

    Declaration::Enum(enum_decl)
}


#[derive(Debug, Default, Clone)]
pub struct UnionDecl {
    pub namespace: Namespace,
    pub annotation_list: Vec<Annotation>,
    pub name: String,
    pub type_params: Vec<String>,
    pub members: Vec<Declaration>,
}

fn parse_union_decl(annotation_list: Vec<Annotation>, pairs: pest::iterators::Pairs<Rule>) -> Declaration {
    let mut union_decl = UnionDecl { annotation_list, .. Default::default() };

    for pair in pairs {
        match pair.as_rule() {
            Rule::qualified_name => {
                union_decl.name = pair.as_str().into();
            }
            Rule::optional_type_params => {
                union_decl.type_params = parse_optional_type_params(pair.into_inner());
            }
            Rule::parcelable_members => {
                union_decl.members = parse_parcelable_members(pair.into_inner());
            }
            _ => unreachable!("Unexpected rule in parse_union_decl(): {}", pair),
        }
    }
    Declaration::Union(union_decl)
}

fn parse_decl(pairs: pest::iterators::Pairs<Rule>) -> Vec<Declaration> {
    let mut annotation_list = Vec::new();
    let mut declarations = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::annotation_list => {
                annotation_list = parse_annotation_list(pair.into_inner());
            }
            Rule::interface_decl => {
                declarations.push(parse_interface_decl(annotation_list.clone(), pair.into_inner()));
            }

            Rule::parcelable_decl => {
                declarations.push(parse_parcelable_decl(annotation_list.clone(), pair.into_inner()));
            }
            Rule::enum_decl => {
                declarations.push(parse_enum_decl(annotation_list.clone(), pair.into_inner()));
            }
            Rule::union_decl => {
                declarations.push(parse_union_decl(annotation_list.clone(), pair.into_inner()));
            }

            _ => unreachable!("Unexpected rule in parse_decl(): {}", pair),
        };
    }

    declarations
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

pub fn parse_document(data: &str) -> Result<Document, Box<dyn Error>> {
    let mut document = Document::new();

    match AIDLParser::parse(Rule::document, data) {
        Ok(pairs) => {
            for pair in pairs {
                match pair.as_rule() {
                    Rule::package => {
                        document.package = Some(pair.into_inner().next().unwrap().as_str().into());
                    },

                    Rule::imports => {
                        for pair in pair.into_inner() {
                            let import = pair.as_str().to_string();
                            let key = match import.rfind('.') {
                                Some(idx) => &import[(idx+1)..],
                                None => &import,
                            };
                            document.imports.insert(key.into(), import);
                        }
                    }

                    Rule::decl => {
                        document.decls.append(&mut parse_decl(pair.into_inner()));
                    }

                    Rule::EOI => {

                    }

                    _ => {
                        unreachable!("Unexpected rule in parse_document(): {}", pair)
                    }
                }
            }

            // println!("{:?}", document);
        },
        Err(err) => {
            panic!("{}", err);
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



#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::fs;
    use super::*;

    fn parse_str(aidl: &str) -> Result<(), Box<dyn Error>> {
        match AIDLParser::parse(Rule::document, aidl) {
            Ok(_res) => {
                println!("Success");
                Ok(())
            },
            Err(err) => {
                println!("{}", err);
                Err(Box::new(err))
            }
        }
    }

    // fn parse_document_test(doc: &str) -> Result<(), Box<dyn Error>> {
    //     parse_document(doc)?;

    //     Ok(())
    // }

    fn parse_dir(path: &Path, parser: fn(data: &str) -> Result<(), Box<dyn Error>>) -> Result<(), Box<dyn Error>> {
        let entries = fs::read_dir(path).unwrap();

        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                parse_dir(&path, parser)?;
            }
            if path.is_file() && path.extension().unwrap_or_default() == "aidl" {
                let unparsed_file = fs::read_to_string(path.clone()).expect("cannot read file");
                // println!("File: {}", path.display());
                parser(&unparsed_file)?;
            }
        }
        Ok(())
    }

    #[test]
    fn test_parse_only() -> Result<(), Box<dyn Error>> {
        parse_dir(Path::new("aidl"), parse_str)
    }

    // #[test]
    // fn test_parse_document() -> Result<(), Box<dyn Error>> {
    //     parse_dir(&Path::new("aidl"), parse_document_test)
    // }

    #[test]
    fn test_parse_string_expr() -> Result<(), Box<dyn Error>> {
        let mut res = AIDLParser::parse(Rule::string_expr, r##""Hello" + " World""##).map_err(|err| {
            println!("{}", err);
            err
        })?;

        let expr = parse_string_expr(res.next().unwrap().into_inner());
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
        let mut res = AIDLParser::parse(Rule::expression, r##"1 + -3 * 2 << 2 | 4"##).map_err(|err| {
            println!("{}", err);
            err
        })?;

        let expr = parse_expression(res.next().unwrap().into_inner());
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

        assert_eq!(expr.calculate(), ConstExpr::new(ValueType::Int64(-20)));

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
}