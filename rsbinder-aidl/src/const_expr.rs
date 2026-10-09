// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Constant-expression folding for AIDL constants and default values.
//!
//! # Arithmetic overflow
//!
//! `arithmetic_basic_op!` performs integer `+ - * / %` in i64 (`/` and `%` are checked, so
//! divide-by-zero becomes a diagnostic instead of a panic), then range-checks the result against
//! the *promoted* operand type. This mirrors AOSP `OverflowGuard<T>`
//! (`aidl_const_expressions.cpp`), which computes in the promoted type with
//! `__builtin_*_overflow` and hard-fails on overflow ("Constant expression computation
//! overflows."). Float/double binary expressions use the plain operator: rsbinder keeps them
//! working, while AOSP rejects them outright (b/313951203).
//!
//! # Shifts
//!
//! The shift amount is range-checked in u64 before narrowing to u32, so `1 << 4294967296` cannot
//! truncate to a 0-bit shift. A negative amount shifts in the other direction (AIDL-defined, AOSP
//! `AidlBinaryConstExpression::evaluate`). Mirroring AOSP `OverflowGuard::operator<<`/`>>`:
//! - an amount `>= sizeof(T)*8` of the promoted type is "Constant expression computation
//!   overflows" (computing in i64 and truncating would fold `1 << 40` to 0);
//! - a negative left operand never shifts (`-8 >> 1` is a diagnostic);
//! - a left shift may move bits into, not past, the sign position: the amount must not exceed the
//!   operand's leading-zero count. `1 << 31` and `1L << 63` are legal, `2 << 31` is a diagnostic.
//!
//! # Narrowing
//!
//! `ConstExpr::convert_to` mirrors AOSP `ValueString` (`aidl_const_expressions.cpp`): a value
//! outside the declared type's range is a build error, not a two's-complement wrap, so
//! `const byte A = 128;` never becomes `-128`. int/long-width hex literals are already wrapped into
//! the signed range at parse time (`0x80000000` is Int32 == INT32_MIN), the AOSP carve-out for bit
//! patterns; byte-width bit patterns need the `u8` suffix (`0xFFu8`), as in AOSP.

use crate::error::ConstExprError;
use crate::parser;

// Bounds `calculate_at_depth` recursion so untrusted `.aidl` nesting cannot overflow the stack.
const MAX_EXPR_DEPTH: usize = 256;

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
                _ => Err(ConstExprError::new(format!(
                    "can't apply bitwise operator '{}' to non-integer type: {} {:?}",
                    $desc, $lhs.raw_expr(), $rhs
                ))),
            }
        }
    }
}

// See the module doc: "Arithmetic overflow" (AOSP `OverflowGuard<T>`).
macro_rules! arithmetic_basic_op {
    ($lhs:expr, $int_op:expr, $float_op:tt, $rhs:expr, $desc:expr, $promoted:expr) => {
        {
            let lhs = $lhs.convert_to($promoted)?;
            let rhs = $rhs.convert_to($promoted)?;
            let int_op = $int_op;

            match $promoted {
                ValueType::Void => Ok(ConstExpr::default()),
                // AOSP accepts only `String + String`; other operators or operands are errors.
                ValueType::String(_) => {
                    if $desc != "+" {
                        Err(ConstExprError::new(format!(
                            "only '+' is supported for strings, not '{}'", $desc
                        )))
                    } else if !matches!($lhs.value, ValueType::String(_))
                        || !matches!($rhs.value, ValueType::String(_))
                    {
                        Err(ConstExprError::new(format!(
                            "cannot concatenate a non-string operand: {} + {}",
                            $lhs.to_value_string(), $rhs.to_value_string()
                        )))
                    } else {
                        let value = format!("{}{}", lhs.to_value_string(), rhs.to_value_string());
                        Ok(ConstExpr::new(ValueType::String(value)))
                    }
                }
                // AOSP rejects char binary operands: `AreCompatibleOperandTypes` has no CHARACTER.
                ValueType::Char(_) => Err(ConstExprError::new(format!(
                    "cannot perform operation '{}' on a char in a constant expression", $desc
                ))),
                // Unreachable from `calc_expr`: `integral_promotion` yields at least Int32.
                ValueType::Byte(_) => {
                    let value = int_op(lhs.to_i64()?, rhs.to_i64()?)?;
                    if value > i8::MAX as i64 || value < i8::MIN as i64 {
                        Err(ConstExprError::new(format!(
                            "constant expression computation overflows ('{}' on byte)", $desc
                        )))
                    } else {
                        Ok(ConstExpr::new(ValueType::Byte(value as _)))
                    }
                }
                ValueType::Int32(_) => {
                    let (a, b) = (lhs.to_i64()?, rhs.to_i64()?);
                    let value = int_op(a, b)?;
                    // `INT32_MIN % -1` overflows in i32 (AOSP OverflowGuard) though i64 gives 0.
                    if value > i32::MAX as i64
                        || value < i32::MIN as i64
                        || ($desc == "%" && a == i32::MIN as i64 && b == -1)
                    {
                        Err(ConstExprError::new(format!(
                            "constant expression computation overflows ('{}' on int)", $desc
                        )))
                    } else {
                        Ok(ConstExpr::new(ValueType::Int32(value as _)))
                    }
                }
                ValueType::Int64(_) => {
                    Ok(ConstExpr::new(ValueType::Int64(int_op(lhs.to_i64()?, rhs.to_i64()?)? as _)))
                }
                ValueType::Float(_) => {
                    Ok(ConstExpr::new(ValueType::Float((lhs.to_f64()? $float_op rhs.to_f64()?) as f32 as _)))
                }
                ValueType::Double(_) => {
                    Ok(ConstExpr::new(ValueType::Double((lhs.to_f64()? $float_op rhs.to_f64()?) as _)))
                }
                ValueType::Bool(_) => {
                    Ok(ConstExpr::new(ValueType::Bool(int_op(lhs.to_i64()?, rhs.to_i64()?)? != 0)))
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
        // Full AIDL enum type. Short enum names can collide across packages.
        // A union's `Tag` carries the union's name here (its module); `enum_name` is `Tag`.
        enum_type: String,
        enum_name: String,
        member_name: String,
        value: i64,
        // AOSP `AidlConstantReference` copies the member value's `final_type_`, not `@Backing`.
        kind: RefKind,
    },
}

/// The AOSP `final_type_` an enum member's value carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Bool,
    Int8,
    Int32,
    Int64,
}

impl RefKind {
    /// The kind of a folded member value; `None` for a non-integral one.
    pub(crate) fn of(value: &ValueType) -> Option<RefKind> {
        Some(match value {
            ValueType::Bool(_) => RefKind::Bool,
            ValueType::Byte(_) => RefKind::Int8,
            ValueType::Int32(_) => RefKind::Int32,
            ValueType::Int64(_) => RefKind::Int64,
            ValueType::Reference { kind, .. } => *kind,
            _ => return None,
        })
    }
}

impl ValueType {
    /// AOSP `IntegralPromotion` of an enum reference's value: `long` stays, anything else is `int`.
    pub(crate) fn promoted_reference(value: i64, kind: RefKind) -> ValueType {
        match i32::try_from(value) {
            Ok(value) if kind != RefKind::Int64 => ValueType::Int32(value),
            _ => ValueType::Int64(value),
        }
    }

    /// An enum reference's value at its own type, as AOSP unary operators see it.
    fn typed_reference(value: i64, kind: RefKind) -> ValueType {
        match kind {
            RefKind::Bool => ValueType::Bool(value != 0),
            RefKind::Int8 => ValueType::Byte(value as i8),
            RefKind::Int32 => ValueType::Int32(value as i32),
            RefKind::Int64 => ValueType::Int64(value),
        }
    }

    #[cfg(test)]
    fn new_expr(lhs: ValueType, operator: &str, rhs: ValueType) -> ValueType {
        ValueType::Expr {
            lhs: Box::new(ConstExpr::new(lhs)),
            operator: operator.into(),
            rhs: Box::new(ConstExpr::new(rhs)),
        }
    }

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
            // AOSP `IsCompatibleType` rejects unary operators on strings.
            ValueType::String(_) => Err(ConstExprError::new(
                "can't apply unary operator '~' to a string",
            )),
            // AOSP `IsCompatibleType` has no CHARACTER case.
            ValueType::Char(_) => Err(ConstExprError::new(
                "can't apply unary operator '~' to a char",
            )),
            ValueType::Void => Ok(ConstExpr::new(self.clone())),
            ValueType::Byte(v) => Ok(ConstExpr::new(ValueType::Byte(!*v))),
            ValueType::Int32(v) => Ok(ConstExpr::new(ValueType::Int32(!*v))),
            ValueType::Int64(v) => Ok(ConstExpr::new(ValueType::Int64(!*v))),
            // AOSP `handleUnary<bool>`: "Bitwise negation of a boolean expression is always true."
            ValueType::Bool(_) => Err(ConstExprError::new(
                "can't apply unary operator '~' to a boolean",
            )),
            ValueType::Reference { value, kind, .. } => {
                ValueType::typed_reference(*value, *kind).unary_not()
            }
            ValueType::Expr { .. } | ValueType::Unary { .. } => {
                let expr = self.calculate()?;
                expr.value.unary_not()
            }
            _ => Err(ConstExprError::new(format!(
                "can't apply unary operator '~' to {self:?}"
            ))),
        }
    }

    /// Logical `!` keeps the operand's integral type (AOSP `handleUnary<T>`); `~` is `unary_not`.
    fn logical_not(&self) -> Result<ConstExpr, ConstExprError> {
        match self {
            ValueType::Expr { .. } | ValueType::Unary { .. } => {
                let expr = self.calculate()?;
                expr.value.logical_not()
            }
            ValueType::Reference { value, kind, .. } => {
                ValueType::typed_reference(*value, *kind).logical_not()
            }
            _ => {
                let b = !self.to_bool()?;
                Ok(ConstExpr::new(match self {
                    ValueType::Byte(_) => ValueType::Byte(b as i8),
                    ValueType::Int32(_) => ValueType::Int32(b as i32),
                    ValueType::Int64(_) => ValueType::Int64(b as i64),
                    _ => ValueType::Bool(b),
                }))
            }
        }
    }

    fn unary_minus(&self) -> Result<ConstExpr, ConstExprError> {
        // AOSP `OverflowGuard::operator-`: negating the type's minimum is a build error.
        fn overflow<T: std::fmt::Display>(v: T) -> ConstExprError {
            ConstExprError::new(format!(
                "constant expression computation overflows: cannot negate {v}"
            ))
        }
        match self {
            // See `unary_not`: AOSP rejects unary operators on strings.
            ValueType::String(_) => Err(ConstExprError::new(
                "can't apply unary operator '-' to a string",
            )),
            ValueType::Char(_) => Err(ConstExprError::new(
                "can't apply unary operator '-' to a char",
            )),
            ValueType::Void => Ok(ConstExpr::new(self.clone())),
            // AOSP `OverflowGuard<bool>`: `-true` stays true, `-false` negates the minimum.
            ValueType::Bool(true) => Ok(ConstExpr::new(ValueType::Bool(true))),
            ValueType::Bool(false) => Err(overflow(false)),
            ValueType::Reference { value, kind, .. } => {
                ValueType::typed_reference(*value, *kind).unary_minus()
            }
            ValueType::Byte(v) => Ok(ConstExpr::new(ValueType::Byte(
                v.checked_neg().ok_or_else(|| overflow(*v))?,
            ))),
            ValueType::Int32(v) => Ok(ConstExpr::new(ValueType::Int32(
                v.checked_neg().ok_or_else(|| overflow(*v))?,
            ))),
            ValueType::Int64(v) => Ok(ConstExpr::new(ValueType::Int64(
                v.checked_neg().ok_or_else(|| overflow(*v))?,
            ))),
            ValueType::Float(v) => Ok(ConstExpr::new(ValueType::Float(-(*v as f32) as _))),
            ValueType::Double(v) => Ok(ConstExpr::new(ValueType::Double(-*v))),
            ValueType::Expr { .. } | ValueType::Unary { .. } => {
                let expr = self.calculate()?;
                expr.value.unary_minus()
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
            ValueType::Char(_) => Err(ConstExprError::new("a char is not a boolean")),
            ValueType::Byte(v) => Ok(*v != 0),
            ValueType::Int32(v) => Ok(*v != 0),
            ValueType::Int64(v) => Ok(*v != 0),
            ValueType::Float(v) | ValueType::Double(v) => Ok(*v != 0.),
            ValueType::Reference { value, .. } => Ok(*value != 0),
            ValueType::Array(_) => Err(ConstExprError::new("to_bool() for Array is not supported")),
            // Typo or missing import: AOSP rejects it, so no fabricated `false`.
            ValueType::Name(name) => resolve_name(name)?.to_bool(),
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
            ValueType::Reference { value, .. } => Ok(*value as _),
            ValueType::Array(_) => Err(ConstExprError::new("to_f64() for Array is not supported")),
            ValueType::Name(name) => resolve_name(name)?.to_f64(),
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
            ValueType::Name(name) => resolve_name(name)?.to_i64(),
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
            '\r' => String::from("\\r"),
            '\0' => String::from("\\0"),
            // Rust has no `\a`/`\b`/`\f`/`\v`; hex-escape like AOSP `PrintCharLiteral`.
            c if c.is_control() || is_bidi_control(c) => format!("\\u{{{:x}}}", c as u32),
            _ => ch.to_string(),
        }
    }

    pub(crate) fn to_init(&self, param: InitParam) -> String {
        match self {
            ValueType::String(s) => {
                let s: String = s
                    .chars()
                    .map(|c| {
                        if is_bidi_control(c) {
                            format!("\\u{{{:x}}}", c as u32)
                        } else {
                            c.to_string()
                        }
                    })
                    .collect();
                if param.is_const {
                    format!("\"{s}\"")
                } else {
                    format!("\"{s}\".into()")
                }
            }
            // Non-finite values (`1.0e400`) need `f32::INFINITY` etc.; `inff32` is not Rust.
            ValueType::Float(v) => {
                let f = *v as f32;
                if f.is_finite() {
                    format!("{f}f32")
                } else if f.is_nan() {
                    "f32::NAN".to_owned()
                } else if f > 0.0 {
                    "f32::INFINITY".to_owned()
                } else {
                    "f32::NEG_INFINITY".to_owned()
                }
            }
            ValueType::Double(v) => {
                if v.is_finite() {
                    format!("{v}f64")
                } else if v.is_nan() {
                    "f64::NAN".to_owned()
                } else if *v > 0.0 {
                    "f64::INFINITY".to_owned()
                } else {
                    "f64::NEG_INFINITY".to_owned()
                }
            }
            ValueType::Char(_) => format!("'{}' as u16", self.to_value_string()),
            ValueType::Name(_) => self.to_value_string(),
            ValueType::Reference {
                enum_type,
                enum_name,
                member_name,
                value,
                ..
            } => {
                if param.is_const {
                    // For constants, always use numeric values
                    format!("{}", value)
                } else {
                    let enum_name = crate::escape_rust_keyword(enum_name);
                    let member_name = crate::escape_rust_keyword(member_name);
                    // `enum_type` is the enum's resolved name: look it up as is, never re-bind it.
                    match parser::lookup_decl_from_canonical(enum_type) {
                        Some(lookup_decl) => {
                            if let Some(path) = parser::builtin_rust_path(&lookup_decl.ns) {
                                return format!(
                                    "{}::{path}::{member_name}",
                                    crate::type_generator::crate_name()
                                );
                            }
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
                // `const T[]` renders as `pub const X: &[T]`; `vec![]` is not a const expr.
                let mut res = if param.is_fixed_array {
                    "[".to_owned()
                } else if param.is_const {
                    "&[".to_owned()
                } else {
                    "vec![".to_owned()
                };
                for v in v {
                    let init_str = match &v.value {
                        // Byte arrays are `u8` in Rust: emit -1 as 255 (AOSP `aidl_to_rust.cpp`).
                        ValueType::Byte(b) => (*b as u8).to_string(),
                        _ => v.value.to_init(param.clone()),
                    };

                    let some_str = if let ValueType::Array(_) = v.value {
                        init_str
                    } else if param.is_nullable {
                        format!("::core::option::Option::Some({init_str})")
                    } else {
                        init_str
                    };

                    res += &(some_str + ",");
                }

                res += "]";

                res
            }
            ValueType::Holder => {
                if param.is_vintf {
                    format!(
                        "{}::ParcelableHolder::new({}::Stability::Vintf)",
                        param.crate_name, param.crate_name
                    )
                } else {
                    "::core::default::Default::default()".to_string()
                }
            }
            ValueType::Byte(_)
            | ValueType::Int32(_)
            | ValueType::Int64(_)
            | ValueType::Bool(_)
            | ValueType::Expr { .. }
            | ValueType::Unary { .. } => self.to_value_string(),

            _ => "::core::default::Default::default()".to_string(),
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
            // No literal form for these; empty string instead of a panic on user input.
            _ => String::new(),
        }
    }

    fn calc_expr(
        lhs: &ConstExpr,
        operator: &str,
        rhs: &ConstExpr,
        depth: usize,
    ) -> Result<ConstExpr, ConstExprError> {
        let lhs = lhs.value.calculate_at_depth(depth + 1)?;
        let rhs = rhs.value.calculate_at_depth(depth + 1)?;
        // AOSP `AreCompatibleOperandTypes` has no CHARACTER case: every binary operator rejects it.
        if matches!(lhs.value, ValueType::Char(_)) || matches!(rhs.value, ValueType::Char(_)) {
            return Err(ConstExprError::new(format!(
                "cannot apply operator '{operator}' to a char in a constant expression"
            )));
        }
        // AOSP `AidlBinaryConstExpression::evaluate`: arrays take no operator, strings only `+`.
        let is = |f: fn(&ValueType) -> bool| f(&lhs.value) || f(&rhs.value);
        if is(|v| matches!(v, ValueType::Array(_))) {
            return Err(ConstExprError::new(format!(
                "Operation '{operator}' is not supported with array literals"
            )));
        }
        if operator != "+" && is(|v| matches!(v, ValueType::String(_))) {
            return Err(ConstExprError::new(format!(
                "only '+' is supported for strings, not '{operator}'"
            )));
        }

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
                // See the module doc: "Shifts" (checked in u64; negative amount flips direction).
                let raw_amount = rhs.to_i64()?;
                let amount: u64 = if raw_amount < 0 {
                    is_shl = !is_shl;
                    raw_amount.unsigned_abs()
                } else {
                    raw_amount as u64
                };

                // AOSP rejects an amount `>= sizeof(T)*8`; i64 math would fold `1 << 40` to 0.
                let bits: u32 = match &promoted {
                    ValueType::Int64(_) => 64,
                    // Int32 / Byte both integral-promote to `int` for the shift.
                    _ => 32,
                };
                if amount >= bits as u64 {
                    // Quote `raw_amount`: `amount` is the magnitude after a sign flip.
                    return Err(ConstExprError::new(format!(
                        "shift amount {raw_amount} out of range for operator '{operator}' \
                         (operand width {bits} bits)"
                    )));
                }
                let rhs_value = amount as u32;

                // See the module doc: "Shifts" (AOSP `OverflowGuard::operator<<`, CLZ bound).
                if lhs_value < 0 {
                    return Err(ConstExprError::new(format!(
                        "constant expression computation overflows: cannot shift the negative \
                         value {lhs_value}"
                    )));
                }
                if is_shl {
                    let clz = if bits == 64 {
                        (lhs_value as u64).leading_zeros()
                    } else {
                        (lhs_value as u32).leading_zeros()
                    };
                    if rhs_value > clz {
                        return Err(ConstExprError::new(format!(
                            "constant expression computation overflows: {lhs_value} << {rhs_value} \
                             does not fit in {bits} bits"
                        )));
                    }
                }

                let value = if is_shl {
                    lhs_value.wrapping_shl(rhs_value)
                } else {
                    lhs_value.wrapping_shr(rhs_value)
                };

                match promoted {
                    ValueType::Int32(_) => Ok(ConstExpr::new(ValueType::Int32(value as _))),
                    ValueType::Int64(_) => Ok(ConstExpr::new(ValueType::Int64(value as _))),
                    ValueType::Byte(_) => Ok(ConstExpr::new(ValueType::Byte(value as _))),
                    _ => Err(ConstExprError::new(format!(
                        "can't apply shift operator '{}' to non-integer type: {}",
                        operator,
                        lhs.raw_expr()
                    ))),
                }
            }
            // Checked ops: see the module doc "Arithmetic overflow".
            "+" => arithmetic_basic_op!(
                lhs,
                |a: i64, b: i64| -> Result<i64, ConstExprError> {
                    a.checked_add(b).ok_or_else(|| {
                        ConstExprError::new("constant expression computation overflows ('+' on long)")
                    })
                },
                +, rhs, "+", &promoted
            ),
            "-" => arithmetic_basic_op!(
                lhs,
                |a: i64, b: i64| -> Result<i64, ConstExprError> {
                    a.checked_sub(b).ok_or_else(|| {
                        ConstExprError::new("constant expression computation overflows ('-' on long)")
                    })
                },
                -, rhs, "-", &promoted
            ),
            "*" => arithmetic_basic_op!(
                lhs,
                |a: i64, b: i64| -> Result<i64, ConstExprError> {
                    a.checked_mul(b).ok_or_else(|| {
                        ConstExprError::new("constant expression computation overflows ('*' on long)")
                    })
                },
                *, rhs, "*", &promoted
            ),
            "/" => arithmetic_basic_op!(
                lhs,
                |a: i64, b: i64| -> Result<i64, ConstExprError> {
                    a.checked_div(b).ok_or_else(|| {
                        ConstExprError::new("division by zero or overflow in constant expression")
                    })
                },
                /, rhs, "/", &promoted
            ),
            "%" => arithmetic_basic_op!(
                lhs,
                |a: i64, b: i64| -> Result<i64, ConstExprError> {
                    a.checked_rem(b).ok_or_else(|| {
                        ConstExprError::new("modulo by zero or overflow in constant expression")
                    })
                },
                %, rhs, "%", &promoted
            ),
            _ => unreachable!(),
        }
    }

    /// Folds in the current scope; a name yields its referent's final value, never its expression.
    pub fn calculate(&self) -> Result<ConstExpr, ConstExprError> {
        self.calculate_at_depth(0)
    }

    /// Names a fold of this expression looks up, in evaluation order.
    pub(crate) fn referenced_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        // Explicit stack: the tree is untrusted and may be deep.
        let mut pending = vec![self];
        while let Some(value) = pending.pop() {
            match value {
                ValueType::Name(name) => names.push(name.clone()),
                ValueType::Unary { expr, .. } => pending.push(&expr.value),
                ValueType::Expr { lhs, rhs, .. } => {
                    pending.push(&rhs.value);
                    pending.push(&lhs.value);
                }
                ValueType::Array(items) => pending.extend(items.iter().rev().map(|i| &i.value)),
                _ => {}
            }
        }
        names
    }

    /// The first name a folded value still holds, i.e. one its scope could not resolve.
    pub(crate) fn unresolved_name(&self) -> Option<&str> {
        match self {
            ValueType::Name(name) => Some(name),
            ValueType::Array(items) => items.iter().find_map(|item| item.value.unresolved_name()),
            _ => None,
        }
    }

    fn calculate_at_depth(&self, depth: usize) -> Result<ConstExpr, ConstExprError> {
        // Deep nesting from untrusted input needs a cap; names never add depth (see `calculate`).
        if depth > MAX_EXPR_DEPTH {
            return Err(ConstExprError::new(
                "constant expression nested too deeply (exceeded recursion limit)",
            ));
        }
        match self {
            ValueType::Unary { operator, expr } => {
                let expr = expr.value.calculate_at_depth(depth + 1)?;
                // AOSP `IsCompatibleType`: no CHARACTER/ARRAY case; FLOATING takes only `+`, `-`.
                let is_float = matches!(expr.value, ValueType::Float(_) | ValueType::Double(_));
                let is_other = matches!(expr.value, ValueType::Char(_) | ValueType::Array(_));
                if is_other || (is_float && operator == "!") {
                    return Err(ConstExprError::new(format!(
                        "can't apply unary operator '{operator}' to {}",
                        expr.to_value_string()
                    )));
                }
                if operator == "-" {
                    expr.value.unary_minus()
                } else if operator == "~" {
                    expr.value.unary_not()
                } else if operator == "!" {
                    expr.value.logical_not()
                } else if matches!(expr.value, ValueType::String(_)) {
                    // Unary `+` on a string: AOSP rejects all unary operators on strings.
                    Err(ConstExprError::new(
                        "can't apply a unary operator to a string",
                    ))
                } else {
                    Ok(expr)
                }
            }
            ValueType::Expr { lhs, operator, rhs } => {
                ValueType::calc_expr(lhs, operator, rhs, depth)
            }
            ValueType::Array(v) => {
                let mut array = Vec::new();

                for value in v {
                    let element = value.value.calculate_at_depth(depth + 1)?;
                    // Copying a nested array lets `{A, A}` chains double the value per link.
                    if let (ValueType::Name(name), ValueType::Array(items)) =
                        (&value.value, &element.value)
                    {
                        if items.iter().any(|i| matches!(i.value, ValueType::Array(_))) {
                            return Err(ConstExprError::new(format!(
                                "array constant '{name}' holds arrays, so it cannot be an \
                                 element of an array literal"
                            )));
                        }
                    }
                    array.push(element);
                }

                Ok(ConstExpr::new(ValueType::Array(array)))
            }
            ValueType::Name(name) => {
                Ok(parser::name_to_const_expr(name)?
                    .unwrap_or_else(|| ConstExpr::new(self.clone())))
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

// A resolved value never holds a `Name` (see `parser::fold_symbol_expr`), so this cannot loop.
fn resolve_name(name: &str) -> Result<ConstExpr, ConstExprError> {
    parser::name_to_const_expr(name)?
        .ok_or_else(|| ConstExprError::new(format!("cannot resolve constant reference '{name}'")))
}

// rustc denies these raw in a literal (`text_direction_codepoint_in_literal`); escape them.
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
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
    // Enum refs promote to their integer (AOSP `AidlConstantReference`); `order()` ranks them top.
    if let ValueType::Reference { value, kind, .. } = value_type {
        return ValueType::promoted_reference(value, kind);
    }
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
        // AIDL `char` is UTF-16 (`u16`); a wider code point would truncate in `as u16`.
        if let (ValueType::Char(c), ValueType::Char(_)) = (&self.value, value_type) {
            if *c as u32 > 0xFFFF {
                return Err(ConstExprError::new(format!(
                    "{:?} is outside the 16-bit range of an AIDL char",
                    c
                )));
            }
        }
        // AOSP `ValueString`: a FLOATING value initializes only a float or double.
        if matches!(self.value, ValueType::Float(_) | ValueType::Double(_))
            && matches!(
                value_type,
                ValueType::Bool(_)
                    | ValueType::Byte(_)
                    | ValueType::Int32(_)
                    | ValueType::Int64(_)
                    | ValueType::Char(_)
            )
        {
            return Err(ConstExprError::new(format!(
                "floating-point value {} cannot initialize an integral, boolean or char type",
                self.to_value_string()
            )));
        }
        // AOSP `ValueString`: a CHARACTER value initializes only a char.
        if matches!(self.value, ValueType::Char(_))
            && matches!(
                value_type,
                ValueType::Bool(_)
                    | ValueType::Byte(_)
                    | ValueType::Int32(_)
                    | ValueType::Int64(_)
                    | ValueType::Float(_)
                    | ValueType::Double(_)
            )
        {
            return Err(ConstExprError::new(format!(
                "char value {} can only initialize a char",
                self.to_value_string()
            )));
        }
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
                // AOSP `ValueString`: only a STRING value initializes a String.
                ValueType::String(_) => Err(ConstExprError::new(format!(
                    "{} is not a string and cannot initialize a String",
                    self.to_value_string()
                ))),
                // See the module doc: "Narrowing" (AOSP `ValueString` range checks).
                ValueType::Byte(_) => {
                    let v = self.to_i64()?;
                    if v > i8::MAX as i64 || v < i8::MIN as i64 {
                        return Err(ConstExprError::new(format!(
                            "value {v} is out of range for byte (-128..=127); for a bit \
                             pattern, use the u8 suffix (e.g. 0xFFu8)"
                        )));
                    }
                    Ok(ConstExpr::new(ValueType::Byte(v as i8 as _)))
                }
                ValueType::Int32(_) => {
                    let v = self.to_i64()?;
                    if v > i32::MAX as i64 || v < i32::MIN as i64 {
                        return Err(ConstExprError::new(format!(
                            "value {v} is out of range for int; for a bit pattern, use a \
                             hex literal or the u32 suffix"
                        )));
                    }
                    Ok(ConstExpr::new(ValueType::Int32(v as i32 as _)))
                }
                ValueType::Int64(_) => Ok(ConstExpr::new(ValueType::Int64(self.to_i64()?))),
                ValueType::Float(_) => {
                    Ok(ConstExpr::new(ValueType::Float(self.to_f64()? as f32 as _)))
                }
                ValueType::Double(_) => Ok(ConstExpr::new(ValueType::Double(self.to_f64()?))),
                ValueType::Bool(_) => Ok(ConstExpr::new(ValueType::Bool(self.to_bool()?))),
                ValueType::Char(_) => {
                    // `u16`: AIDL char width; `char::from_u32` also rejects surrogates.
                    let raw = self.to_i64()?;
                    let ch = u16::try_from(raw)
                        .ok()
                        .and_then(|v| char::from_u32(v.into()))
                        .ok_or_else(|| {
                            ConstExprError::new(format!("{raw} is not a valid char code point"))
                        })?;
                    Ok(Self::new(ValueType::Char(ch)))
                }
                // Enum targets never reach here; AOSP `ValueString` rejects other defined types.
                ValueType::UserDefined(name) => Err(ConstExprError::new(format!(
                    "{} cannot initialize the non-enum type {name}",
                    self.to_value_string()
                ))),
                ValueType::Reference { .. } => Ok(self.clone()),
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

    #[test]
    fn test_division_by_zero_is_error_not_panic() {
        let expr = ValueType::new_expr(ValueType::Int32(1), "/", ValueType::Int32(0));
        assert!(expr.calculate().is_err());

        let expr = ValueType::new_expr(ValueType::Int32(5), "%", ValueType::Int32(0));
        assert!(expr.calculate().is_err());
    }

    #[test]
    fn test_integer_overflow_is_diagnostic() {
        // AOSP OverflowGuard: overflow in the promoted type is a diagnostic, never a wrap.
        let expr = ValueType::new_expr(ValueType::Int64(i64::MAX), "+", ValueType::Int64(1));
        assert!(
            expr.calculate().is_err(),
            "i64::MAX + 1 must be a diagnostic"
        );

        let expr = ValueType::new_expr(ValueType::Int32(i32::MAX), "+", ValueType::Int32(1));
        assert!(
            expr.calculate().is_err(),
            "int32 overflow must be a diagnostic"
        );

        let expr = ValueType::new_expr(ValueType::Int64(i64::MIN), "/", ValueType::Int64(-1));
        assert!(
            expr.calculate().is_err(),
            "i64::MIN / -1 must be a diagnostic"
        );

        // In-range arithmetic still folds.
        let expr = ValueType::new_expr(ValueType::Int32(i32::MAX), "+", ValueType::Int32(0));
        assert_eq!(
            expr.calculate().unwrap(),
            ConstExpr::new(ValueType::Int32(i32::MAX))
        );
    }

    #[test]
    fn test_shift_overflow_guard_matches_aosp() {
        // Amount == CLZ(lhs) is legal: `1 << 31` is INT32_MIN, `1L << 63` is INT64_MIN.
        let expr = ValueType::new_expr(ValueType::Int32(1), "<<", ValueType::Int32(31));
        assert_eq!(
            expr.calculate().unwrap(),
            ConstExpr::new(ValueType::Int32(i32::MIN))
        );
        let expr = ValueType::new_expr(ValueType::Int64(1), "<<", ValueType::Int32(63));
        assert_eq!(
            expr.calculate().unwrap(),
            ConstExpr::new(ValueType::Int64(i64::MIN))
        );

        // AOSP rejects `2 << 31` (amount > CLZ).
        let expr = ValueType::new_expr(ValueType::Int32(2), "<<", ValueType::Int32(31));
        assert!(expr.calculate().is_err(), "2 << 31 must be a diagnostic");

        // A negative left operand never shifts (AOSP OverflowGuard).
        let expr = ValueType::new_expr(ValueType::Int32(-8), ">>", ValueType::Int32(1));
        assert!(expr.calculate().is_err(), "-8 >> 1 must be a diagnostic");

        // A negative shift amount shifts in the other direction (AIDL-defined).
        let expr = ValueType::new_expr(ValueType::Int32(8), "<<", ValueType::Int32(-1));
        assert_eq!(
            expr.calculate().unwrap(),
            ConstExpr::new(ValueType::Int32(4))
        );
    }

    #[test]
    fn test_char_binary_operand_is_diagnostic() {
        // AOSP `AreCompatibleOperandTypes` has no CHARACTER case.
        let expr = ValueType::new_expr(ValueType::Char('a'), "+", ValueType::Int32(1));
        assert!(expr.calculate().is_err(), "'a' + 1 must be a diagnostic");
    }

    #[test]
    fn test_string_concat_requires_plus_and_strings() {
        let expr = ValueType::new_expr(
            ValueType::String("a".into()),
            "+",
            ValueType::String("b".into()),
        );
        assert_eq!(
            expr.calculate().unwrap(),
            ConstExpr::new(ValueType::String("ab".into()))
        );
        let expr = ValueType::new_expr(
            ValueType::String("a".into()),
            "-",
            ValueType::String("b".into()),
        );
        assert!(
            expr.calculate().is_err(),
            "\"a\" - \"b\" must be a diagnostic"
        );
    }

    #[test]
    fn test_narrowing_out_of_range_is_diagnostic() {
        // AOSP rejects `byte A = 128` and decimal int overflow; hex wraps at parse time.
        assert!(ConstExpr::new(ValueType::Int32(128))
            .convert_to(&ValueType::Byte(0))
            .is_err());
        assert!(ConstExpr::new(ValueType::Int64(2_147_483_648))
            .convert_to(&ValueType::Int32(0))
            .is_err());
        // In-range narrowing still converts.
        assert_eq!(
            ConstExpr::new(ValueType::Int32(127))
                .convert_to(&ValueType::Byte(0))
                .unwrap(),
            ConstExpr::new(ValueType::Byte(127))
        );
    }

    #[test]
    fn test_double_conversion_preserves_f64_precision() {
        // 0.1 + 0.2 is not exact in f32; converting to Double must keep the full f64 value.
        let value = 0.1_f64 + 0.2_f64;
        let converted = ConstExpr::new(ValueType::Double(value))
            .convert_to(&ValueType::Double(0.0))
            .unwrap();
        assert_eq!(converted, ConstExpr::new(ValueType::Double(value)));
    }

    // Array.to_bool() returns Err (not panic)
    #[test]
    fn test_array_to_bool_returns_error() {
        let arr = ValueType::Array(vec![ConstExpr::new(ValueType::Int32(1))]);
        let result = arr.to_bool();
        assert!(result.is_err());
    }

    // AOSP `ValueString`: a CHARACTER value is never a boolean, whatever its code point.
    #[test]
    fn test_char_to_bool_returns_error() {
        assert!(ValueType::Char('\0').to_bool().is_err());
        assert!(ValueType::Char('a').to_bool().is_err());
    }

    // AOSP `evaluate`/`IsCompatibleType`: strings take only `+`; arrays take no operator.
    #[test]
    fn string_and_array_operands_take_no_comparison_or_unary() {
        let s = |v: &str| ValueType::String(v.into());
        let arr = || ValueType::Array(vec![ConstExpr::new(ValueType::Bool(true))]);
        for op in ["==", "!=", "<", "&&"] {
            assert!(
                ValueType::new_expr(s("a"), op, s("b")).calculate().is_err(),
                "{op}"
            );
            assert!(
                ValueType::new_expr(arr(), op, arr()).calculate().is_err(),
                "{op}"
            );
        }
        assert!(ValueType::new_expr(s("a"), "+", s("b")).calculate().is_ok());
        for op in ["-", "~", "!", "+"] {
            let unary = ValueType::Unary {
                operator: op.into(),
                expr: Box::new(ConstExpr::new(arr())),
            };
            assert!(unary.calculate().is_err(), "{op}");
        }
    }

    // Array.to_i64() returns Err (not panic)
    #[test]
    fn test_array_to_i64_returns_error() {
        let arr = ValueType::Array(vec![ConstExpr::new(ValueType::Int32(1))]);
        let result = arr.to_i64();
        assert!(result.is_err());
    }

    // Array.to_f64() returns Err (not panic)
    #[test]
    fn test_array_to_f64_returns_error() {
        let arr = ValueType::Array(vec![ConstExpr::new(ValueType::Int32(1))]);
        let result = arr.to_f64();
        assert!(result.is_err());
    }

    // Thousands of parens in untrusted `.aidl` must be a diagnostic, not a stack overflow.
    #[test]
    fn deeply_nested_expr_returns_error_not_stack_overflow() {
        let mut e = ConstExpr::new(ValueType::Int32(1));
        for _ in 0..(MAX_EXPR_DEPTH + 16) {
            e = ConstExpr::new(ValueType::Unary {
                operator: "~".to_string(),
                expr: Box::new(e),
            });
        }
        assert!(e.value.calculate().is_err());
    }

    // A modest nesting depth (well under the limit) must still fold normally.
    #[test]
    fn moderately_nested_expr_still_evaluates() {
        let mut e = ConstExpr::new(ValueType::Int32(0));
        for _ in 0..8 {
            e = ConstExpr::new(ValueType::Unary {
                operator: "~".to_string(),
                expr: Box::new(e),
            });
        }
        assert!(e.value.calculate().is_ok());
    }
}
