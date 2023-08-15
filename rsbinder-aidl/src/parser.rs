
use std::sync::Mutex;
use std::collections::HashMap;
use std::error::Error;

use convert_case::{Case, Casing};

use pest::Parser;
#[derive(pest_derive::Parser)]
#[grammar = "aidl.pest"]
pub struct AIDLParser;

use crate::const_expr::{ConstExpr, StringExpr, Expression};
use crate::DEFAULT_NAMESPACE;

lazy_static! {
    static ref DECLARATION_MAP: Mutex<HashMap<String, Declaration>> = Mutex::new(HashMap::new());
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
    pub const_expr: ConstExpr,
}

impl VariableDecl {
    pub fn to_string(&self) -> String {
        if self.constant {
            // public constant string type must be "&str".
            let mut type_string = self.r#type.to_string(true);
            if type_string == "str" {
                type_string = "&str".into();
            }
            format!("pub const {}: {} = {};\n", self.identifier.to_case(Case::UpperSnake), type_string, self.const_expr.to_string())
        } else {
            format!("pub {}: {},\n", self.identifier(), self.r#type.to_string(false))
        }
    }

    pub fn to_enum_member(&self) -> String {
        format!("{}({}),", self.member_identifier(), self.member_type())
    }

    pub fn to_default(&self) -> String {
        format!("{}: {},\n", self.identifier(), self.r#type.to_default())
    }

    pub fn identifier(&self) -> String {
        self.identifier.to_case(Case::Snake)
    }

    pub fn member_identifier(&self) -> String {
        self.identifier.to_case(Case::UpperCamel)
    }

    pub fn member_type(&self) -> String {
        self.r#type.to_string(false)
    }
}

#[derive(Debug, Default, Clone)]
pub struct InterfaceDecl {
    pub namespace: String,
    pub annotation_list: Vec<Annotation>,
    pub oneway: bool,
    pub name: String,
    pub method_list: Vec<MethodDecl>,
    pub constant_list: Vec<VariableDecl>,
    pub members: Vec<Declaration>,
}

impl InterfaceDecl {
    pub fn post_process(&mut self) -> Result<(), crate::const_expr::Error>  {
        let mut dict = HashMap::new();
        for decl in &self.constant_list {
            dict.insert(decl.identifier.clone(), decl.const_expr.clone());
        }

        for decl in &mut self.constant_list {
            if decl.identifier == "DUMP_FLAG_PRIORITY_ALL" {
                print!("DUMP_FLAG_PRIORITY_ALL: {:?}", decl);
            }
            decl.const_expr = decl.const_expr.calculate(&mut dict)?;
        }

        Ok(())
    }
}

#[derive(Debug, Default, Clone)]
pub struct ParcelableDecl {
    pub namespace: String,
    pub name: String,
    pub type_params: Vec<String>,
    pub cpp_header: String,
    pub members: Vec<Declaration>,
}

impl ParcelableDecl {
    pub fn post_process(&mut self) -> Result<(), crate::const_expr::Error>  {
        let mut dict = HashMap::new();
        for decl in &mut self.members {
            match decl {
                Declaration::Interface(decl) => { decl.post_process()?; }
                Declaration::Parcelable(decl) => { decl.post_process()?; }
                Declaration::Variable(decl) => {
                    dict.insert(decl.identifier.clone(), decl.const_expr.clone());
                },
                _ => {}
            }
        }

        for decl in &mut self.members {
            if let Declaration::Variable(decl) = decl {
                decl.const_expr = decl.const_expr.calculate(&mut dict)?;
            }
        }

        Ok(())
    }
}


#[derive(Debug, Default, Clone)]
pub struct Arg {
    pub direction: String,
    pub r#type: Type,
    pub identifier: String,
}

impl Arg {
    pub fn to_string(&self) -> (String, String) {
        let mutable = if self.direction == "out" || self.direction == "inout" {
            "mut"
        } else { "" };

        let borrowed = if self.r#type.is_clonable() { "" } else { "&" };

        let param = format!("_arg_{}", self.identifier.to_case(Case::Snake));
        let arg = format!("{}: {}{}{}", param.clone(), borrowed, mutable, self.r#type.to_string(true));
        (param, arg)
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
            Some(&decl)
        } else {
            None
        }
    }

    pub fn namespace(&self) -> &str {
        match self {
            Declaration::Parcelable(decl) => &decl.namespace,
            Declaration::Interface(decl) => &decl.namespace,
            Declaration::Enum(decl) => &decl.namespace,
            Declaration::Union(decl) => &decl.namespace,
            _ => todo!(),
        }
    }

    pub fn namespace_mut(&mut self) -> &mut String {
        match self {
            Declaration::Parcelable(decl) => &mut decl.namespace,
            Declaration::Interface(decl) => &mut decl.namespace,
            Declaration::Enum(decl) => &mut decl.namespace,
            Declaration::Union(decl) => &mut decl.namespace,
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

fn generic_type_args_to_string(args: &Vec<Type>, is_arg: bool) -> String {
    let mut args_str = String::new();

    args.iter().for_each(|t| {
        args_str.push_str(", ");
        args_str.push_str(&t.to_string(is_arg));
    });

    args_str[2..].into()
}

impl Generic {
    pub fn to_string(&self, is_arg: bool) -> String {
        match self {
            Generic::Type1 { type_args1, non_array_type, type_args2 } => {
                format!("{}{}{}",
                    generic_type_args_to_string(type_args1, is_arg),
                    non_array_type.to_string(is_arg),
                    generic_type_args_to_string(&type_args2, is_arg))
            }
            Generic::Type2 { non_array_type, type_args } => {
                format!("{}{}",
                    non_array_type.to_string(is_arg),
                    generic_type_args_to_string(&type_args, is_arg))
            }
            Generic::Type3 { type_args } => {
                generic_type_args_to_string(&type_args, is_arg)
            }
        }
    }
}


#[derive(Debug, Default, Clone)]
pub struct NonArrayType {
    pub name: String,
    pub generic: Option<Box<Generic>>,
}

impl NonArrayType {
    pub fn to_string(&self, is_arg: bool) -> String {
        let gen_str = match &self.generic {
            Some(gen) => gen.to_string(is_arg),
            None => "".into(),
        };
        match self.name.as_str() {
            "List" => format!("Vec<{}>", gen_str),
            _ => format!("{}{}", type_to_rust(&self.name, is_arg).type_name, gen_str),
        }
    }

    fn is_clonable(&self) -> bool {
        type_to_rust(&self.name, true).is_clonable
    }

    fn is_declared(&self) -> bool {
        type_to_rust(&self.name, true).is_declared
    }

    fn to_default(&self) -> String {
        type_to_rust(&self.name, true).default
    }
}

#[derive(Debug, Default, Clone)]
pub struct ArrayType {
    const_expr: Option<ConstExpr>,
}

impl ArrayType {
    pub fn to_string(&self) -> String {
        match &self.const_expr {
            Some(expr) => format!("[{}]", expr.to_string()),
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
    pub fn to_string(&self, is_arg: bool) -> String {
        let mut res = self.non_array_type.to_string(is_arg);
        // TODO: implement Vector.....
        // &Vec<T>
        self.array_types.iter().for_each(|t| {
            if t.is_vector() {
                res = format!("Vec<{res}>");
            } else {
                res += &t.to_string();
            }
        });

        if is_nullable(&self.annotation_list) {
            format!("Option<{res}>")
        } else {
            res
        }
    }

    pub fn to_default(&self) -> String {
        self.non_array_type.to_default()
    }

    pub fn is_declared(&self) -> bool {
        self.non_array_type.is_declared()
    }

    pub fn is_clonable(&self) -> bool {
        if self.annotation_list.is_empty() && self.array_types.is_empty() {
            return self.non_array_type.is_clonable()
        }
        false
    }
}

pub struct TypeToRust {
    type_name: String,
    default: String,
    is_clonable: bool,
    is_declared: bool,
}

fn type_to_rust(type_name: &str, is_arg: bool) -> TypeToRust {
    // Return is (rust type, default, clonable).
    let zero = "0".to_owned();
    let zero_f = "0.0".to_owned();
    let default = "Default::default()".to_owned();
    let mut is_declared = false;
    let res = match type_name {
        "boolean" => ("bool".to_owned(), "false".to_owned(), true),
        "byte" => ("i8".to_owned(), zero, true),
        "char" => ("u16".to_owned(), zero, true),
        "int" => ("i32".to_owned(), zero, true),
        "long" => ("i64".to_owned(), zero, true),
        "float" => ("f32".to_owned(), zero_f, true),
        "double" => ("f64".to_owned(), zero_f, true),
        "void" => ("()".to_owned(), default, true),
        "String" => {
            if is_arg == true {
                ("str".to_owned(), default, false)
            } else {
                ("String".to_owned(), default, true)
            }
        }
        "IBinder" => { ("rsbinder::StrongIBinder".to_owned(), default, true) }
        _ => {
            if let Some(decl) = DECLARATION_MAP.lock().unwrap().get(type_name) {
                let type_name = format!("crate::{}::{}", decl.namespace(), type_name);

                match decl {
                    Declaration::Interface(_) => {
                        is_declared = true;
                        (format!("Arc<dyn {}>", type_name), default, true)
                    }
                    _ => { (type_name.to_owned(), default, false) }
                }
            } else {
                (type_name.to_owned(), default, false)
            }
        }
    };

    TypeToRust {
        type_name: res.0,
        default: res.1,
        is_clonable: res.2,
        is_declared,
    }
}

pub fn is_nullable(annotation_list: &Vec<Annotation>) -> bool {
    for annotation in annotation_list {
        if annotation.annotation == "@nullable" {
            return true
        }
    }

    false
}

pub fn get_backing_type(annotation_list: &Vec<Annotation>) -> String {
    // parse "@Backing(type="byte")"
    for annotation in annotation_list {
        if annotation.annotation == "@Backing" {
            for param in &annotation.parameter_list {
                if param.identifier == "type" {
                    if let ConstExpr::String(expr) = &param.const_expr {
                        if let StringExpr::CStr(cstr) = expr {
                            // The cstr is enclosed in quotes.
                            return type_to_rust(cstr.trim_matches('"'), false).type_name
                        }
                    }
                }
            }
        }
    }

    "i8".into()
}

fn parse_unary(mut pairs: pest::iterators::Pairs<Rule>) -> Expression {
    let operator = pairs.next().unwrap().as_str().to_owned();
    let factor = parse_factor(pairs.next().unwrap().into_inner().next().unwrap());
    Expression::Unary {
        operator: operator,
        expr: Box::new(factor),
    }
}

fn parse_intvalue(arg_value: &str, radix: u32) -> Expression {
    let mut is_u8 = false;

    let value = if radix == 16 && (arg_value.starts_with("0x") || arg_value.starts_with("0X")) {
        &arg_value[2..]
    } else {
        arg_value
    };

    let value = if value.ends_with("l") || value.ends_with("L") {
        &value[..value.len() -1]
    } else if value.ends_with("u8") {
        is_u8 = true;
        &value[..value.len()-2]
    } else {
        value
    };

    let value = i64::from_str_radix(&value, radix).map_err(|err| {
        eprintln!("{:?}\nparse_intvalue() - Invalid int value: {}, radix: {}\n", err, arg_value, radix);
        err
    }).unwrap();
    if is_u8 == true {
        Expression::IntU8(value as u8)
    } else {
        Expression::Int(value)
    }
}

fn parse_value(pair: pest::iterators::Pair<Rule>) -> Expression {
    match pair.as_rule() {

        // Rule::const_expr => { parse_const_expr(pair.into_inner()) }
        Rule::qualified_name => { Expression::Name(pair.as_str().into()) }
        // Rule::C_STR => { ConstExpr::CStr(pair.as_str().into()) }
        Rule::HEXVALUE => { parse_intvalue(pair.as_str(), 16) }
        Rule::FLOATVALUE => {
            let value = pair.as_str();
            let value = if value.ends_with("f") {
                &value[..value.len()-1]
            } else {
                value
            };

            Expression::Float(value.parse::<f64>().unwrap())
        }
        Rule::INTVALUE => { parse_intvalue(pair.as_str(), 10) }
        Rule::TRUE_LITERAL => { Expression::Bool(true) }
        Rule::FALSE_LITERAL => { Expression::Bool(false) }
        _ => unreachable!("Unexpected rule in parse_value(): {}", pair),
    }
}

fn parse_factor(pair: pest::iterators::Pair<Rule>) -> Expression {
    // println!("parse_factor {:?}", pair);
    match pair.as_rule() {
        Rule::expression => {
            parse_expression(pair.into_inner())
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

fn parse_expression_term(pair: pest::iterators::Pair<Rule>) -> Expression {
    // println!("expression_term {:?}", pair);
    match pair.as_rule() {
        Rule::equality | Rule::comparison | Rule::bitwise | Rule::shift | Rule::arith |
        Rule::logical => {
            parse_expression(pair.into_inner())
        }
        Rule::factor => {
            parse_factor(pair.into_inner().next().unwrap())
        }
        _ => unreachable!("Unexpected rule in Rule::parse_expression_into: {}", pair),
    }
}

fn parse_expression(mut pairs: pest::iterators::Pairs<Rule>) -> Expression {
    let mut lhs = parse_expression_term(pairs.next().unwrap());

    while let Some(pair) = pairs.next() {
        let op = pair.as_str().to_owned();
        let rhs = parse_expression_term(pairs.next().unwrap());

        lhs = Expression::Expr { lhs: Box::new(lhs), operator: op, rhs: Box::new(rhs) };
    }

    lhs
}

fn parse_string_term(pair: pest::iterators::Pair<Rule>) -> StringExpr {
    match pair.as_rule() {
        Rule::C_STR => { StringExpr::CStr(pair.as_str().into())}
        Rule::qualified_name => { StringExpr::Name(pair.as_str().into())}
        _ => unreachable!("Unexpected rule in Rule::parse_string_term: {}", pair),
    }
}

fn parse_string_expr(pairs: pest::iterators::Pairs<Rule>) -> StringExpr {
    let mut expr_list = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::string_term => {
                expr_list.push(Box::new(parse_string_term(pair.into_inner().next().unwrap())));
            }
            _ => unreachable!("Unexpected rule in Rule::parse_string_expr: {}", pair),
        }
    }

    if expr_list.len() > 1 {
        StringExpr::List(expr_list)
    } else {
        *expr_list.pop().unwrap()
    }
}

fn parse_const_expr(pair: pest::iterators::Pair<Rule>) -> ConstExpr {
    match pair.as_rule() {
        Rule::constant_value_list => {
            let mut value_list = Vec::new();
            for pair in pair.into_inner() {
                match pair.as_rule() {
                    Rule::const_expr => {
                        value_list.push(Box::new(parse_const_expr(pair.into_inner().next().unwrap())));
                    }
                    _ => unreachable!("Unexpected rule in Rule::constant_value_list: {}", pair),
                }
            }
            ConstExpr::List(value_list)
        }

        Rule::CHARVALUE => {
            ConstExpr::Char(pair.as_str().chars().nth(0).unwrap())
        }

        Rule::expression => {
            ConstExpr::Expression(parse_expression(pair.into_inner()))
        }

        Rule::string_expr => {
            ConstExpr::String(parse_string_expr(pair.into_inner()))
        }

        _ => unreachable!("Unexpected rule in parse_const_expr(): {}", pair),
    }
}

fn parse_parameter(pairs: pest::iterators::Pairs<Rule>) -> Parameter {
    let mut parameter = Parameter {
        identifier: "".to_string(),
        const_expr: ConstExpr::Expression(Expression::Int(0)),
    };

    for pair in pairs {
        match pair.as_rule() {
            Rule::identifier => { parameter.identifier = pair.as_str().into(); }
            Rule::const_expr => { parameter.const_expr = parse_const_expr(pair.into_inner().next().unwrap()); }
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
            Rule::const_expr => { array_type.const_expr = Some(parse_const_expr(pair.into_inner().next().unwrap())); }
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
                    Some(pair) => decl.const_expr = parse_const_expr(pair),
                    None => decl.const_expr = ConstExpr::List(vec![]),
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
            Rule::direction => { arg.direction = pair.as_str().into(); }
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
                let expr = parse_intvalue(pair.as_str(), 10);
                decl.intvalue = match expr.calculate(&mut HashMap::new()).unwrap() {
                    Expression::Int(v) => v,
                    Expression::IntU8(v) => v as i64,
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

fn parse_parcelable_decl(pairs: pest::iterators::Pairs<Rule>) -> Declaration {
    let mut parcelable = ParcelableDecl::default();

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
    pub namespace: String,
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
            Rule::const_expr => { res.const_expr = Some(parse_const_expr(pair.into_inner().next().unwrap())); }
            _ => unreachable!("Unexpected rule in parse_enumerator(): {}", pair),
        }
    }

    res
}

fn parse_enum_decl(annotation_list: Vec<Annotation>, pairs: pest::iterators::Pairs<Rule>) -> Declaration {
    let mut enum_decl = EnumDecl { annotation_list, .. Default::default() };

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
    pub namespace: String,
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
                declarations.push(parse_parcelable_decl(pair.into_inner()));
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

pub fn calculate_namespace(decl: &mut Declaration, namespace: Vec<String>) {
    if decl.is_variable().is_some() {
        return;
    }

    let mut curr_ns = DEFAULT_NAMESPACE.to_owned();

    for ns in &namespace {
        curr_ns += &("::".to_owned() + &ns);
    }

    *decl.namespace_mut() = curr_ns;

    DECLARATION_MAP.lock().unwrap().insert(decl.name().to_owned(), decl.clone());

    let mut new_ns = namespace.clone();
    new_ns.push(decl.name().to_owned());

    for mut decl in decl.members_mut() {
        calculate_namespace(&mut decl, new_ns.clone());
    }
}

pub fn parse_document(data: &str) -> Result<Document, Box<dyn Error>> {
    let mut document = Document::new();

    match AIDLParser::parse(Rule::document, &data) {
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
                        println!("main loop: {}", pair);
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
        vec![package.replace(".", "::")]
    } else {
        Vec::new()
    };

    for mut decl in &mut document.decls {
        calculate_namespace(&mut decl, namespace.clone());
    }

    Ok(document)
}



#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::fs;
    use super::*;

    fn parse_str(aidl: &str) -> Result<(), Box<dyn Error>> {
        match AIDLParser::parse(Rule::document, &aidl) {
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

    fn parse_document_test(doc: &str) -> Result<(), Box<dyn Error>> {
        parse_document(doc)?;

        Ok(())
    }

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
        parse_dir(&Path::new("aidl"), parse_str)
    }

    #[test]
    fn test_parse_document() -> Result<(), Box<dyn Error>> {
        parse_dir(&Path::new("aidl"), parse_document_test)
    }

    #[test]
    fn test_parse_string_expr() -> Result<(), Box<dyn Error>> {
        match AIDLParser::parse(Rule::string_expr, r##""Hello" + " World""##) {
            Ok(mut res) => {
                assert_eq!(
                    parse_string_expr(res.next().unwrap().into_inner()),
                    StringExpr::List(vec![
                        Box::new(StringExpr::CStr("\"Hello\"".to_string())),
                        Box::new(StringExpr::CStr("\" World\"".to_string()))
                    ])
                );
                Ok(())
            },
            Err(err) => {
                println!("{}", err);
                Err(Box::new(err))
            }
        }
    }

    #[test]
    fn test_parse_expression() -> Result<(), Box<dyn Error>> {
        let mut res = AIDLParser::parse(Rule::expression, r##"1 + -3 * 2 << 2 | 4"##).map_err(|err| {
            println!("{}", err);
            err
        })?;

        let expr = parse_expression(res.next().unwrap().into_inner());
        assert_eq!(
            expr.clone(),
            Expression::Expr {
                lhs: Box::new(Expression::Expr {
                    lhs: Box::new(Expression::Expr {
                        lhs: Box::new(Expression::Int(1)),
                        operator: "+".to_string(),
                        rhs: Box::new(Expression::Expr {
                            lhs: Box::new(Expression::Unary {
                                operator: "-".to_string(),
                                expr: Box::new(Expression::Int(3))
                            }),
                            operator: "*".to_string(),
                            rhs: Box::new(Expression::Int(2))
                        })
                    }),
                    operator: "<<".to_string(),
                    rhs: Box::new(Expression::Int(2))
                }),
                operator: "|".to_string(),
                rhs: Box::new(Expression::Int(4))
            },
        );

        assert_eq!(expr.calculate(&mut HashMap::new())?, Expression::Int(-20));

        Ok(())
    }
}