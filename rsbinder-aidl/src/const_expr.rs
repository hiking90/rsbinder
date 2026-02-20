// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use crate::error::ConstExprError;
use crate::parser;

macro_rules! arithmetic_bit_op {
    ($lhs:expr, $op:tt, $rhs:expr, $desc:expr, $promoted:expr) => {
        {
            match $promoted {
                ValueType::Bool(_) => {
                    let value = ($lhs.to_i64()? $op $rhs.to_i64()?) != 0;
                    Ok(ConstExpr::new(ValueType::Bool(value)))
                }
                ValueType::Byte(_) => {
                    let value = ($lhs.to_i64()? $op $rhs.to_i64()?);
                    Ok(ConstExpr::new(ValueType::Byte(value as _)))
                }
                ValueType::Int32(_) => {
                    let value = ($lhs.to_i64()? $op $rhs.to_i64()?);
                    Ok(ConstExpr::new(ValueType::Int32(value as _)))
                }
                ValueType::Int64(_) => {
                    let value = ($lhs.to_i64()? $op $rhs.to_i64()?);
                    Ok(ConstExpr::new(ValueType::Int64(value as _)))
                }
                ValueType::Reference { .. } => {
                    let value = ($lhs.to_i64()? $op $rhs.to_i64()?);
                    Ok(ConstExpr::new(ValueType::Int64(value as _)))
                }
                _ => Err(ConstExprError::new(format!(
                    "can't apply bitwise operator '{}' to non-integer type: {} {:?}",
                    $desc, $lhs.raw_expr(), $rhs
                ))),
            }
        }
    }
}

macro_rules! arithmetic_basic_op {
    ($lhs:expr, $op:tt, $rhs:expr, $desc:expr, $promoted:expr) => {
        {
            let lhs = $lhs.convert_to($promoted)?;
            let rhs = $rhs.convert_to($promoted)?;

            match $promoted {
                ValueType::Void => Ok(ConstExpr::default()),
                ValueType::String(_) | ValueType::Char(_) => {
                    let value = format!("{}{}", lhs.to_value_string(), rhs.to_value_string());
                    Ok(ConstExpr::new(ValueType::String(value)))
                }
                ValueType::Byte(_) => {
                    Ok(ConstExpr::new(ValueType::Byte((lhs.to_i64()? $op rhs.to_i64()?) as u8 as _)))
                }
                ValueType::Int32(_) => {
                    Ok(ConstExpr::new(ValueType::Int32((lhs.to_i64()? $op rhs.to_i64()?) as i32 as _)))
                }
                ValueType::Int64(_) => {
                    Ok(ConstExpr::new(ValueType::Int64((lhs.to_i64()? $op rhs.to_i64()?) as _)))
                }
                ValueType::Float(_) => {
                    Ok(ConstExpr::new(ValueType::Float((lhs.to_f64()? $op rhs.to_f64()?) as f32 as _)))
                }
                ValueType::Double(_) => {
                    Ok(ConstExpr::new(ValueType::Double((lhs.to_f64()? $op rhs.to_f64()?) as _)))
                }
                ValueType::Bool(_) => {
                    Ok(ConstExpr::new(ValueType::Bool((lhs.to_i64()? $op rhs.to_i64()?) != 0)))
                }
                ValueType::Reference { .. } => {
                    Ok(ConstExpr::new(ValueType::Int64((lhs.to_i64()? $op rhs.to_i64()?) as _)))
                }
                _ => {
                    Err(ConstExprError::new(format!(
                        "can't apply operator '{}' to non-integer or float type: {} {} {}",
                        $desc, lhs.raw_expr(), $desc, rhs.raw_expr()
                    )))
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InitParam {
    pub is_const: bool,
    pub is_fixed_array: bool,
    pub is_nullable: bool,
    pub is_vintf: bool,
    pub crate_name: String,
}

impl InitParam {
    pub(crate) fn builder() -> Self {
        Self {
            is_const: false,
            is_fixed_array: false,
            is_nullable: false,
            is_vintf: false,
            crate_name: "rsbinder".into(),
        }
    }

    pub(crate) fn with_const(mut self, is_const: bool) -> Self {
        self.is_const = is_const;
        self
    }

    pub(crate) fn with_fixed_array(mut self, is_fixed_array: bool) -> Self {
        self.is_fixed_array = is_fixed_array;
        self
    }

    pub(crate) fn with_nullable(mut self, is_nullable: bool) -> Self {
        self.is_nullable = is_nullable;
        self
    }

    pub(crate) fn with_vintf(mut self, is_vintf: bool) -> Self {
        self.is_vintf = is_vintf;
        self
    }

    pub(crate) fn with_crate_name(mut self, crate_name: &str) -> Self {
        self.crate_name = crate_name.to_owned();
        self
    }
}

#[derive(Default, Debug, Clone)]
pub enum ValueType {
    #[default]
    Void,
    Name(String),
    Bool(bool),
    Byte(i8),
    Int32(i32),
    Int64(i64),
    Char(char),
    String(String),
    Float(f64),
    Double(f64),
    Array(Vec<ConstExpr>),
    Map(Box<ConstExpr>, Box<ConstExpr>),
    Expr {
        lhs: Box<ConstExpr>,
        operator: String,
        rhs: Box<ConstExpr>,
    },
    Unary {
        operator: String,
        expr: Box<ConstExpr>,
    },
    IBinder,
    FileDescriptor,
    Holder,
    UserDefined(String),
    Reference {
        enum_name: String,
        member_name: String,
        value: i64,
    },
}

impl ValueType {
    #[cfg(test)]
    fn new_expr(lhs: ValueType, operator: &str, rhs: ValueType) -> ValueType {
        ValueType::Expr {
            lhs: Box::new(ConstExpr::new(lhs)),
            operator: operator.into(),
            rhs: Box::new(ConstExpr::new(rhs)),
        }
    }

    // fn new_unary(operator: &str, expr: ValueType) -> ValueType {
    //     ValueType::Unary{
    //         operator: operator.into(),
    //         expr: Box::new(ConstExpr::new(expr)),
    //     }
    // }

    pub fn is_primitive(&self) -> bool {
        matches!(
            self,
            ValueType::Void
                | ValueType::Bool(_)
                | ValueType::Byte(_)
                | ValueType::Int32(_)
                | ValueType::Int64(_)
                | ValueType::Char(_)
                | ValueType::Float(_)
                | ValueType::Double(_)
                | ValueType::Reference { .. }
        )
    }

    fn order(&self) -> u32 {
        match self {
            ValueType::Void => 0,
            ValueType::Name(_) => 1,
            ValueType::Bool(_) => 2,
            ValueType::Byte(_) => 3,
            ValueType::Int32(_) => 4,
            ValueType::Int64(_) => 5,
            ValueType::Char(_) => 6,
            ValueType::String(_) => 7,
            ValueType::Float(_) => 8,
            ValueType::Double(_) => 9,
            ValueType::Array(_) => 10,
            ValueType::Map(_, _) => 11,
            ValueType::Expr { .. } => 12,
            ValueType::Unary { .. } => 13,
            ValueType::IBinder => 14,
            ValueType::FileDescriptor => 15,
            ValueType::Holder => 16,
            ValueType::UserDefined(_) => 17,
            ValueType::Reference { .. } => 18,
        }
    }

    fn unary_not(&self) -> Result<ConstExpr, ConstExprError> {
        match self {
            ValueType::Void | ValueType::String(_) | ValueType::Char(_) => {
                Ok(ConstExpr::new(self.clone()))
            }
            ValueType::Byte(v) => Ok(ConstExpr::new(ValueType::Byte(!*v))),
            ValueType::Int32(v) => Ok(ConstExpr::new(ValueType::Int32(!*v))),
            ValueType::Int64(v) => Ok(ConstExpr::new(ValueType::Int64(!*v))),
            ValueType::Bool(v) => Ok(ConstExpr::new(ValueType::Bool(!*v))),
            ValueType::Expr { .. } | ValueType::Unary { .. } => {
                let expr = self.calculate()?;
                expr.value.unary_not()
            }
            ValueType::Array(v) => {
                let mut list = Vec::new();
                for expr in v {
                    list.push(expr.value.unary_not()?)
                }
                Ok(ConstExpr::new(ValueType::Array(list)))
            }
            _ => Err(ConstExprError::new(format!(
                "can't apply unary operator '~' or '!' to {self:?}"
            ))),
        }
    }

    fn unary_minus(&self) -> Result<ConstExpr, ConstExprError> {
        match self {
            ValueType::Void | ValueType::String(_) | ValueType::Bool(_) | ValueType::Char(_) => {
                Ok(ConstExpr::new(self.clone()))
            }
            ValueType::Byte(v) => Ok(ConstExpr::new(ValueType::Byte((*v).wrapping_neg() as _))),
            ValueType::Int32(v) => Ok(ConstExpr::new(ValueType::Int32((*v).wrapping_neg() as _))),
            ValueType::Int64(v) => Ok(ConstExpr::new(ValueType::Int64(v.wrapping_neg()))),
            ValueType::Float(v) => Ok(ConstExpr::new(ValueType::Float(-(*v as f32) as _))),
            ValueType::Double(v) => Ok(ConstExpr::new(ValueType::Double(-*v))),
            ValueType::Expr { .. } | ValueType::Unary { .. } => {
                let expr = self.calculate()?;
                expr.value.unary_minus()
            }

            ValueType::Array(v) => {
                let mut list = Vec::new();
                for expr in v {
                    list.push(expr.value.unary_minus()?)
                }

                Ok(ConstExpr::new(ValueType::Array(list)))
            }
            _ => Err(ConstExprError::new(format!(
                "can't apply unary operator '-' to {self:?}"
            ))),
        }
    }

    pub fn to_bool(&self) -> Result<bool, ConstExprError> {
        match self {
            ValueType::Void => Ok(false),
            ValueType::String(_) => {
                Err(ConstExprError::new("to_bool() for String is not supported"))
            }
            ValueType::Bool(v) => Ok(*v),
            ValueType::Char(_) => Ok(true),
            ValueType::Byte(v) => Ok(*v != 0),
            ValueType::Int32(v) => Ok(*v != 0),
            ValueType::Int64(v) => Ok(*v != 0),
            ValueType::Float(v) | ValueType::Double(v) => Ok(*v != 0.),
            ValueType::Array(_) => Err(ConstExprError::new("to_bool() for Array is not supported")),
            ValueType::Name(name) => {
                let expr = parser::name_to_const_expr(name);
                match expr {
                    Some(expr) => {
                        let calculated = expr.calculate()?;
                        if let ValueType::Name(_) = calculated.value {
                            Ok(false)
                        } else {
                            calculated.to_bool()
                        }
                    }
                    None => Ok(false),
                }
            }
            ValueType::Expr { .. } | ValueType::Unary { .. } => {
                let expr = self.calculate()?;
                expr.to_bool()
            }
            _ => Err(ConstExprError::new(format!(
                "to_bool() not supported for {self:?}"
            ))),
        }
    }

    pub fn to_f64(&self) -> Result<f64, ConstExprError> {
        match self {
            ValueType::Void => Ok(0.),
            ValueType::String(_) => {
                Err(ConstExprError::new("to_f64() for String is not supported"))
            }
            ValueType::Bool(v) => Ok(if *v { 1.0 } else { 0.0 }),
            ValueType::Char(v) => Ok(*v as i64 as _),
            ValueType::Byte(v) => Ok(*v as _),
            ValueType::Int32(v) => Ok(*v as _),
            ValueType::Int64(v) => Ok(*v as _),
            ValueType::Float(v) | ValueType::Double(v) => Ok(*v as _),
            ValueType::Array(_) => Err(ConstExprError::new("to_f64() for Array is not supported")),
            ValueType::Name(name) => {
                let expr = parser::name_to_const_expr(name);
                match expr {
                    Some(expr) => {
                        let calculated = expr.calculate()?;
                        if let ValueType::Name(_) = calculated.value {
                            Ok(0.0)
                        } else {
                            calculated.to_f64()
                        }
                    }
                    None => Ok(0.0),
                }
            }
            ValueType::Expr { .. } | ValueType::Unary { .. } => {
                let expr = self.calculate()?;
                expr.to_f64()
            }
            _ => Err(ConstExprError::new(format!(
                "to_f64() not supported for {self:?}"
            ))),
        }
    }

    pub fn to_i64(&self) -> Result<i64, ConstExprError> {
        match self {
            ValueType::Void => Ok(0),
            ValueType::String(_) => {
                Err(ConstExprError::new("to_i64() for String is not supported"))
            }
            ValueType::Bool(v) => Ok(*v as _),
            ValueType::Char(v) => Ok(*v as _),
            ValueType::Byte(v) => Ok(*v as _),
            ValueType::Int32(v) => Ok(*v as _),
            ValueType::Int64(v) => Ok(*v as _),
            ValueType::Float(v) | ValueType::Double(v) => Ok(*v as _),
            ValueType::Array(_) => Err(ConstExprError::new(format!(
                "to_i64() for Array is not supported: {self:?}"
            ))),
            ValueType::Name(name) => {
                let expr = parser::name_to_const_expr(name);
                match expr {
                    Some(expr) => {
                        let calculated = expr.calculate()?;
                        if let ValueType::Name(_) = calculated.value {
                            Ok(0)
                        } else {
                            calculated.to_i64()
                        }
                    }
                    None => Ok(0),
                }
            }
            ValueType::Reference { value, .. } => Ok(*value),
            ValueType::Expr { .. } | ValueType::Unary { .. } => {
                let expr = self.calculate()?;
                expr.to_i64()
            }
            _ => Err(ConstExprError::new(format!(
                "to_i64() not supported for {self:?}"
            ))),
        }
    }

    fn char_to_string(ch: char) -> String {
        match ch {
            '\\' => String::from("\\\\"),
            '\'' => String::from("\\'"),
            '\"' => String::from("\\\""),
            '\n' => String::from("\\n"),
            '\t' => String::from("\\t"),
            // ... add other special characters as needed ...
            _ => ch.to_string(),
        }
    }

    pub(crate) fn to_init(&self, param: InitParam) -> String {
        match self {
            ValueType::String(_) => {
                if param.is_const {
                    format!("\"{}\"", self.to_value_string())
                } else {
                    format!("\"{}\".into()", self.to_value_string())
                }
            }
            ValueType::Float(_) => format!("{}f32", self.to_value_string()),
            ValueType::Double(_) => format!("{}f64", self.to_value_string()),
            ValueType::Char(_) => format!("'{}' as u16", self.to_value_string()),
            ValueType::Name(_) => self.to_value_string(),
            ValueType::Reference {
                enum_name,
                member_name,
                value,
            } => {
                if param.is_const {
                    // For constants, always use numeric values
                    format!("{}", value)
                } else {
                    // Use proper namespace resolution for cross-package enum references
                    match parser::lookup_decl_from_name(enum_name, crate::Namespace::AIDL) {
                        Some(lookup_decl) => {
                            let curr_ns = parser::current_namespace();
                            let ns = curr_ns.relative_mod(&lookup_decl.ns);
                            if !ns.is_empty() {
                                format!("{}::{}::{}", ns, enum_name, member_name)
                            } else {
                                format!("{}::{}", enum_name, member_name)
                            }
                        }
                        None => {
                            format!("{}::{}", enum_name, member_name)
                        }
                    }
                }
            }
            ValueType::Array(v) => {
                let mut res = if param.is_fixed_array {
                    "[".to_owned()
                } else {
                    "vec![".to_owned()
                };
                for v in v {
                    let init_str = v.value.to_init(param.clone());

                    let some_str = if let ValueType::Array(_) = v.value {
                        init_str
                    } else if param.is_nullable {
                        format!("Some({init_str})")
                    } else {
                        init_str
                    };

                    res += &(some_str + ",");
                }

                res += "]";

                res
            }
            ValueType::Holder => {
                println!("Init Holder: {param:?}");
                if param.is_vintf {
                    format!(
                        "{}::ParcelableHolder::new({}::Stability::Vintf)",
                        param.crate_name, param.crate_name
                    )
                } else {
                    "Default::default()".to_string()
                }
            }
            ValueType::Byte(_)
            | ValueType::Int32(_)
            | ValueType::Int64(_)
            | ValueType::Bool(_)
            | ValueType::Expr { .. }
            | ValueType::Unary { .. } => self.to_value_string(),

            _ => "Default::default()".to_string(),
        }
    }

    pub fn to_value_string(&self) -> String {
        match self {
            ValueType::Void => "".into(),
            ValueType::String(v) => v.clone(),
            ValueType::Byte(v) => v.to_string(),
            ValueType::Int32(v) => v.to_string(),
            ValueType::Int64(v) => v.to_string(),
            ValueType::Float(v) => (*v as f32).to_string(),
            ValueType::Double(v) => (*v).to_string(),
            ValueType::Bool(v) => v.to_string(),
            ValueType::Char(v) => Self::char_to_string(*v),
            ValueType::Array(v) => {
                let mut res = "vec![".to_owned();
                for v in v {
                    res += &(v.to_value_string() + ",");
                }

                res += "]";

                res
            }
            ValueType::Name(v) => v.to_string(),
            ValueType::Reference {
                enum_name,
                member_name,
                ..
            } => {
                format!("{}.{}", enum_name, member_name)
            }
            ValueType::Expr { lhs, operator, rhs } => {
                format!(
                    "{} {} {}",
                    lhs.to_value_string(),
                    operator,
                    rhs.to_value_string()
                )
            }
            ValueType::Unary { operator, expr } => {
                format!("{} {}", operator, expr.to_value_string())
            }
            _ => unimplemented!(),
        }
    }

    fn calc_expr(
        lhs: &ConstExpr,
        operator: &str,
        rhs: &ConstExpr,
    ) -> Result<ConstExpr, ConstExprError> {
        let lhs = lhs.calculate()?;
        let rhs = rhs.calculate()?;

        let promoted = type_conversion(
            integral_promotion(lhs.value.clone()),
            integral_promotion(rhs.value.clone()),
        );

        match operator {
            "||" => Ok(ConstExpr::new(ValueType::Bool(
                lhs.to_bool()? || rhs.to_bool()?,
            ))),
            "&&" => Ok(ConstExpr::new(ValueType::Bool(
                lhs.to_bool()? && rhs.to_bool()?,
            ))),
            "|" => {
                arithmetic_bit_op!(lhs, |, rhs, "|", promoted)
            }
            "^" => {
                arithmetic_bit_op!(lhs, ^, rhs, "^", promoted)
            }
            "&" => {
                arithmetic_bit_op!(lhs, &, rhs, "&", promoted)
            }
            "==" | "!=" | "<=" | ">=" | "<" | ">" => {
                let lhs = lhs.convert_to(&promoted)?;
                let rhs = rhs.convert_to(&promoted)?;

                let value = match operator {
                    "==" => lhs == rhs,
                    "!=" => lhs != rhs,
                    "<=" => lhs <= rhs,
                    ">=" => lhs >= rhs,
                    "<" => lhs < rhs,
                    ">" => lhs > rhs,
                    _ => unreachable!(),
                };

                Ok(ConstExpr::new(ValueType::Bool(value)))
            }

            "<<" | ">>" => {
                let mut is_shl = operator == "<<";

                let lhs_value = lhs.to_i64()?;
                let rhs_value = rhs.to_i64()?;
                let rhs_value: u32 = if rhs_value < 0 {
                    is_shl = !is_shl;
                    rhs_value.wrapping_neg() as _
                } else {
                    rhs_value as _
                };

                let value = if is_shl {
                    lhs_value.wrapping_shl(rhs_value)
                } else {
                    lhs_value.wrapping_shr(rhs_value)
                };

                match promoted {
                    ValueType::Int32(_) => Ok(ConstExpr::new(ValueType::Int32(value as _))),
                    ValueType::Int64(_) => Ok(ConstExpr::new(ValueType::Int64(value as _))),
                    ValueType::Byte(_) => Ok(ConstExpr::new(ValueType::Byte(value as _))),
                    ValueType::Reference { .. } => Ok(ConstExpr::new(ValueType::Int64(value as _))),
                    _ => Err(ConstExprError::new(format!(
                        "can't apply shift operator '{}' to non-integer type: {}",
                        operator,
                        lhs.raw_expr()
                    ))),
                }
            }
            "+" => arithmetic_basic_op!(lhs, +, rhs, "+", &promoted),
            "-" => arithmetic_basic_op!(lhs, -, rhs, "-", &promoted),
            "*" => arithmetic_basic_op!(lhs, *, rhs, "*", &promoted),
            "/" => arithmetic_basic_op!(lhs, /, rhs, "/", &promoted),
            "%" => arithmetic_basic_op!(lhs, %, rhs, "%", &promoted),
            _ => unreachable!(),
        }
    }

    pub fn calculate(&self) -> Result<ConstExpr, ConstExprError> {
        self.calculate_with_visited(&mut std::collections::HashSet::new())
    }

    fn calculate_with_visited(
        &self,
        visited: &mut std::collections::HashSet<String>,
    ) -> Result<ConstExpr, ConstExprError> {
        match self {
            ValueType::Unary { operator, expr } => {
                let expr = expr.value.calculate_with_visited(visited)?;
                if operator == "-" {
                    expr.value.unary_minus()
                } else if operator == "~" || operator == "!" {
                    expr.value.unary_not()
                } else {
                    Ok(expr)
                }
            }
            ValueType::Expr { lhs, operator, rhs } => ValueType::calc_expr(lhs, operator, rhs),
            ValueType::Array(v) => {
                let mut array = Vec::new();

                for value in v {
                    array.push(value.value.calculate_with_visited(visited)?);
                }

                Ok(ConstExpr::new(ValueType::Array(array)))
            }
            ValueType::Name(name) => {
                if visited.contains(name) {
                    Ok(ConstExpr::new(ValueType::Int32(0)))
                } else {
                    visited.insert(name.clone());
                    let expr = parser::name_to_const_expr(name);
                    match expr {
                        Some(expr) => expr.value.calculate_with_visited(visited),
                        None => Ok(ConstExpr::new(self.clone())),
                    }
                }
            }
            ValueType::Reference { .. } => Ok(ConstExpr::new(self.clone())),
            _ => Ok(ConstExpr::new(self.clone())),
        }
    }
}

impl PartialEq for ValueType {
    fn eq(&self, rhs: &Self) -> bool {
        self.partial_cmp(rhs) == Some(std::cmp::Ordering::Equal)
    }
}

impl PartialOrd for ValueType {
    fn partial_cmp(&self, rhs: &Self) -> Option<std::cmp::Ordering> {
        match self {
            ValueType::Void => {
                if let ValueType::Void = rhs {
                    Some(std::cmp::Ordering::Equal)
                } else {
                    Some(std::cmp::Ordering::Less)
                }
            }
            ValueType::String(v) | ValueType::Name(v) => v.partial_cmp(&rhs.to_value_string()),
            ValueType::Byte(v) => rhs.to_i64().ok().and_then(|r| v.partial_cmp(&(r as _))),
            ValueType::Int32(v) => rhs.to_i64().ok().and_then(|r| v.partial_cmp(&(r as _))),
            ValueType::Int64(v) => rhs.to_i64().ok().and_then(|r| v.partial_cmp(&(r as _))),
            ValueType::Char(v) => rhs.to_i64().ok().and_then(|r| (*v as i64).partial_cmp(&r)),
            ValueType::Bool(v) => rhs.to_bool().ok().and_then(|r| v.partial_cmp(&r)),
            ValueType::Float(v) | ValueType::Double(v) => {
                rhs.to_f64().ok().and_then(|r| v.partial_cmp(&r))
            }
            ValueType::Array(lhs_array) => {
                if let ValueType::Array(rhs_array) = rhs {
                    lhs_array.partial_cmp(rhs_array)
                } else {
                    None
                }
            }
            ValueType::Unary { .. } | ValueType::Expr { .. } => {
                match (self.calculate(), rhs.calculate()) {
                    (Ok(lhs), Ok(rhs)) => lhs.partial_cmp(&rhs),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

fn type_conversion(lhs: ValueType, rhs: ValueType) -> ValueType {
    if lhs.order() == rhs.order() {
        lhs
    } else if let ValueType::Bool(_) = lhs {
        rhs
    } else if let ValueType::Bool(_) = rhs {
        lhs
    } else if lhs.order() > rhs.order() {
        lhs
    } else {
        rhs
    }
}

fn integral_promotion(value_type: ValueType) -> ValueType {
    let i32_order = ValueType::Int32(0).order();
    let value_order = value_type.order();

    if value_order > i32_order {
        value_type
    } else {
        ValueType::Int32(0)
    }
}

impl PartialEq for ConstExpr {
    fn eq(&self, rhs: &Self) -> bool {
        self.partial_cmp(rhs) == Some(std::cmp::Ordering::Equal)
    }
}

impl PartialOrd for ConstExpr {
    fn partial_cmp(&self, rhs: &Self) -> Option<std::cmp::Ordering> {
        self.value.partial_cmp(&rhs.value)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConstExpr {
    pub raw_expr: String,
    pub is_calculated: bool,

    pub value: ValueType,
}

impl ConstExpr {
    pub fn new(value: ValueType) -> Self {
        Self {
            value,
            ..Default::default()
        }
    }

    pub fn new_expr(lhs: ConstExpr, operator: &str, rhs: ConstExpr) -> Self {
        Self {
            value: ValueType::Expr {
                lhs: Box::new(lhs),
                operator: operator.into(),
                rhs: Box::new(rhs),
            },
            ..Default::default()
        }
    }

    pub fn new_unary(operator: &str, expr: ConstExpr) -> Self {
        Self {
            value: ValueType::Unary {
                operator: operator.into(),
                expr: Box::new(expr),
            },
            ..Default::default()
        }
    }

    pub fn set_raw_expr(&mut self, raw_expr: &str) {
        self.raw_expr = raw_expr.into();
    }

    pub fn raw_expr(&self) -> &str {
        &self.raw_expr
    }

    pub fn to_value_string(&self) -> String {
        self.value.to_value_string()
    }

    pub fn to_i64(&self) -> Result<i64, ConstExprError> {
        self.value.to_i64()
    }

    pub fn to_f64(&self) -> Result<f64, ConstExprError> {
        self.value.to_f64()
    }

    pub fn to_bool(&self) -> Result<bool, ConstExprError> {
        self.value.to_bool()
    }

    pub fn convert_to(&self, value_type: &ValueType) -> Result<ConstExpr, ConstExprError> {
        if self.value.order() == value_type.order() {
            Ok(self.clone())
        } else if let ValueType::Array(list) = &self.value {
            let mut res = Vec::new();

            for v in list {
                res.push(v.convert_to(value_type)?)
            }
            Ok(ConstExpr::new(ValueType::Array(res)))
        } else {
            match value_type {
                ValueType::Void => Ok(Self::default()),
                ValueType::String(_) => {
                    Ok(ConstExpr::new(ValueType::String(self.to_value_string())))
                }
                ValueType::Byte(_) => {
                    Ok(ConstExpr::new(ValueType::Byte(self.to_i64()? as i8 as _)))
                }
                ValueType::Int32(_) => {
                    Ok(ConstExpr::new(ValueType::Int32(self.to_i64()? as i32 as _)))
                }
                ValueType::Int64(_) => Ok(ConstExpr::new(ValueType::Int64(self.to_i64()?))),
                ValueType::Float(_) => {
                    Ok(ConstExpr::new(ValueType::Float(self.to_f64()? as f32 as _)))
                }
                ValueType::Double(_) => Ok(ConstExpr::new(ValueType::Float(self.to_f64()?))),
                ValueType::Bool(_) => Ok(ConstExpr::new(ValueType::Bool(self.to_bool()?))),
                ValueType::Char(_) => {
                    let ch = self.to_i64()? as u32;
                    if let Some(ch) = char::from_u32(ch) {
                        Ok(Self::new(ValueType::Char(ch as _)))
                    } else {
                        Err(ConstExprError::new(format!("0x{ch:x} is invalid unicode")))
                    }
                }
                ValueType::UserDefined(_) | ValueType::Reference { .. } => Ok(self.clone()),
                _ => Err(ConstExprError::new(format!(
                    "convert_to: unsupported conversion {:?} -> {:?}",
                    self.value, value_type
                ))),
            }
        }
    }

    pub fn calculate(&self) -> Result<ConstExpr, ConstExprError> {
        if self.is_calculated {
            Ok(self.clone())
        } else {
            let mut expr = self.value.calculate()?;
            expr.is_calculated = true;
            Ok(expr)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expression_arithmatic() {
        let expr = ValueType::new_expr(ValueType::Int32(10), "+", ValueType::Int32(10));

        assert_eq!(
            expr.calculate().unwrap(),
            ConstExpr::new(ValueType::Int32(20))
        );

        let expr = ValueType::new_expr(ValueType::Byte(1), "<<", ValueType::Byte(31));

        assert_eq!(
            expr.calculate().unwrap(),
            ConstExpr::new(ValueType::Int32(0x80000000u32 as _))
        );

        // assert_eq!(expr.calculate(&mut dict), Expression::Int32(100));

        let expr = ValueType::new_expr(ValueType::Byte(10), "/", ValueType::Float(2.0));

        assert_eq!(
            expr.calculate().unwrap(),
            ConstExpr::new(ValueType::Float(5.0))
        );

        let expr = ValueType::new_expr(ValueType::Float(10.0), "%", ValueType::Float(2.0));

        assert_eq!(
            expr.calculate().unwrap(),
            ConstExpr::new(ValueType::Float(10.0 % 2.0))
        );

        let expr = ValueType::new_expr(ValueType::Int32(10), "%", ValueType::Bool(true));

        assert_eq!(
            expr.calculate().unwrap(),
            ConstExpr::new(ValueType::Int32(0))
        );
    }

    // #[test]
    // fn test_const_expr_name() {
    //     let mut dict = HashMap::new();
    //     dict.insert("DUMP_FLAG_PRIORITY_CRITICAL".to_owned(),
    //         ConstExpr::new(
    //             ValueType::new_expr(
    //                 ValueType::Int32(1),
    //                 "<<",
    //                 ValueType::Int32(0),
    //             )
    //         )
    //     );

    //     dict.insert("DUMP_FLAG_PRIORITY_HIGH".to_owned(),
    //         ConstExpr::new(
    //             ValueType::new_expr(
    //                 ValueType::Int32(1),
    //                 "<<",
    //                 ValueType::Int32(1),
    //             )
    //         )
    //     );

    //     dict.insert("DUMP_FLAG_PRIORITY_NORMAL".to_owned(),
    //         ConstExpr::new(
    //             ValueType::new_expr(
    //                 ValueType::Int32(1),
    //                 "<<",
    //                 ValueType::Int32(2),
    //             )
    //         )
    //     );

    //     dict.insert("DUMP_FLAG_PRIORITY_DEFAULT".to_owned(),
    //         ConstExpr::new(
    //             ValueType::new_expr(
    //                 ValueType::Int32(1),
    //                 "<<",
    //                 ValueType::Int32(3),
    //             )
    //         )
    //     );

    //     let expr = ConstExpr::new(
    //         ValueType::new_expr(
    //             ValueType::Name("DUMP_FLAG_PRIORITY_CRITICAL".into()),
    //             "|",
    //             ValueType::new_expr(
    //                 ValueType::Name("DUMP_FLAG_PRIORITY_HIGH".into()),
    //                 "|",
    //                 ValueType::new_expr(
    //                     ValueType::Name("DUMP_FLAG_PRIORITY_NORMAL".into()),
    //                     "|",
    //                     ValueType::Name("DUMP_FLAG_PRIORITY_DEFAULT".into()),
    //                 )
    //             ),
    //         )
    //     );

    //     assert_eq!(expr.calculate(), ConstExpr::new(ValueType::Int32(15)));
    // }

    // 4.3g: Array.to_bool() returns Err (not panic)
    #[test]
    fn test_array_to_bool_returns_error() {
        let arr = ValueType::Array(vec![ConstExpr::new(ValueType::Int32(1))]);
        let result = arr.to_bool();
        assert!(result.is_err());
    }

    // 4.3h: Array.to_i64() returns Err (not panic)
    #[test]
    fn test_array_to_i64_returns_error() {
        let arr = ValueType::Array(vec![ConstExpr::new(ValueType::Int32(1))]);
        let result = arr.to_i64();
        assert!(result.is_err());
    }

    // 4.3i: Array.to_f64() returns Err (not panic)
    #[test]
    fn test_array_to_f64_returns_error() {
        let arr = ValueType::Array(vec![ConstExpr::new(ValueType::Int32(1))]);
        let result = arr.to_f64();
        assert!(result.is_err());
    }
}
