// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use crate::error::ConstExprError;
use crate::parser;

/// Maximum structural nesting depth for constant-expression evaluation.
///
/// `.aidl` source is untrusted input to the compiler; a pathologically
/// nested expression (e.g. thousands of parentheses) would otherwise
/// recurse `calculate_with_visited` until the thread stack overflows and
/// aborts the process. Real AIDL constants nest only a handful of levels,
/// so this bound is far above any legitimate input while staying well
/// below the frame budget that triggers a stack overflow.
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

// `$int_op` performs the arithmetic in i64 (`/`/`%` are checked so
// divide-by-zero becomes a diagnostic instead of a panic); the result is then
// range-checked against the *promoted* operand type, mirroring AOSP's
// `OverflowGuard<T>` (aidl_const_expressions.cpp) which computes `+ - * / %`
// in the promoted type with `__builtin_*_overflow` and hard-fails on
// overflow ("Constant expression computation overflows."). `$float_op` is
// the plain operator for f32/f64 — rsbinder intentionally keeps float
// binary expressions working (AOSP rejects them outright, b/313951203).
macro_rules! arithmetic_basic_op {
    ($lhs:expr, $int_op:expr, $float_op:tt, $rhs:expr, $desc:expr, $promoted:expr) => {
        {
            let lhs = $lhs.convert_to($promoted)?;
            let rhs = $rhs.convert_to($promoted)?;
            let int_op = $int_op;

            match $promoted {
                ValueType::Void => Ok(ConstExpr::default()),
                // AOSP only accepts `String + String` as a string expression;
                // any other operator (or a non-string operand promoted into a
                // string context, e.g. `"a" + 'c'`) is a build error.
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
                // AOSP rejects char operands in binary expressions
                // (`AreCompatibleOperandTypes` has no CHARACTER case). The
                // old string-concat fold silently produced e.g. `"a1"` for
                // `'a' + 1` — a wrong, non-compiling constant.
                ValueType::Char(_) => Err(ConstExprError::new(format!(
                    "cannot perform operation '{}' on a char in a constant expression", $desc
                ))),
                // Defensive: binary operands integral-promote past Byte
                // (`integral_promotion` yields Int32 minimum), so this arm is
                // unreachable from `calc_expr`.
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
                    // `INT32_MIN % -1` overflows in the promoted width (AOSP
                    // OverflowGuard<int32_t>) even though the i64 remainder
                    // (0) is in range; `/` is caught by the range check.
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
                ValueType::Reference { .. } => {
                    Ok(ConstExpr::new(ValueType::Int64(int_op(lhs.to_i64()?, rhs.to_i64()?)? as _)))
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
        enum_type: String,
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
            // AOSP `IsCompatibleType` rejects unary operators on strings; a
            // silent pass-through would emit the operand unchanged.
            ValueType::String(_) => Err(ConstExprError::new(
                "can't apply unary operator '~' to a string",
            )),
            ValueType::Void | ValueType::Char(_) => Ok(ConstExpr::new(self.clone())),
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
                "can't apply unary operator '~' to {self:?}"
            ))),
        }
    }

    /// Logical negation (`!`). Unlike `~` (bitwise complement) this yields
    /// a boolean: `!5 == false` (0 when coerced to an integer target) and
    /// `!0 == true` (1) — matching AIDL/C++ semantics. Routed separately
    /// from [`unary_not`](Self::unary_not) (which is `~`) so an integer `!`
    /// is not mistakenly bit-complemented.
    fn logical_not(&self) -> Result<ConstExpr, ConstExprError> {
        match self {
            ValueType::Expr { .. } | ValueType::Unary { .. } => {
                let expr = self.calculate()?;
                expr.value.logical_not()
            }
            ValueType::Array(v) => {
                let mut list = Vec::new();
                for expr in v {
                    list.push(expr.value.logical_not()?)
                }
                Ok(ConstExpr::new(ValueType::Array(list)))
            }
            _ => Ok(ConstExpr::new(ValueType::Bool(!self.to_bool()?))),
        }
    }

    fn unary_minus(&self) -> Result<ConstExpr, ConstExprError> {
        // Checked negation mirrors AOSP `OverflowGuard::operator-`: negating
        // the type's minimum has no representable result and is a build error
        // instead of a silent wrap back to itself.
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
            ValueType::Void | ValueType::Bool(_) | ValueType::Char(_) => {
                Ok(ConstExpr::new(self.clone()))
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
                        if let ValueType::Name(n) = calculated.value {
                            // Still a name after resolution ⇒ the chain
                            // dead-ends on an unresolvable reference. AOSP
                            // rejects this; folding to `false` would bake a
                            // fabricated constant into the generated code.
                            Err(ConstExprError::new(format!(
                                "cannot resolve constant reference '{n}'"
                            )))
                        } else {
                            calculated.to_bool()
                        }
                    }
                    // Genuinely unresolvable (typo / missing import). AOSP
                    // rejects this at build time; surface a diagnostic instead
                    // of fabricating `false`.
                    None => Err(ConstExprError::new(format!(
                        "cannot resolve constant reference '{name}'"
                    ))),
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
                        if let ValueType::Name(n) = calculated.value {
                            // See `to_bool`: dead-end resolution ⇒ diagnostic.
                            Err(ConstExprError::new(format!(
                                "cannot resolve constant reference '{n}'"
                            )))
                        } else {
                            calculated.to_f64()
                        }
                    }
                    // See `to_bool`: unresolvable reference ⇒ diagnostic, not 0.
                    None => Err(ConstExprError::new(format!(
                        "cannot resolve constant reference '{name}'"
                    ))),
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
                        if let ValueType::Name(n) = calculated.value {
                            // See `to_bool`: dead-end resolution ⇒ diagnostic.
                            Err(ConstExprError::new(format!(
                                "cannot resolve constant reference '{n}'"
                            )))
                        } else {
                            calculated.to_i64()
                        }
                    }
                    // See `to_bool`: unresolvable reference ⇒ diagnostic, not 0.
                    None => Err(ConstExprError::new(format!(
                        "cannot resolve constant reference '{name}'"
                    ))),
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
            '\r' => String::from("\\r"),
            '\0' => String::from("\\0"),
            // Any other control / non-printable character has no single-
            // character Rust escape (`\a`/`\b`/`\f`/`\v` do not exist in
            // Rust), so emit a unicode escape — otherwise the raw byte lands
            // inside a `'...'` char literal and the generated code fails to
            // compile. AOSP `PrintCharLiteral` likewise hex-escapes these.
            c if c.is_control() => format!("\\u{{{:x}}}", c as u32),
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
            // A non-finite fold (e.g. `1.0e400` parses to infinity) would emit
            // `inff32` / `NaNf64` — not valid Rust. Emit the proper float
            // constant instead; finite values keep the suffixed-decimal form.
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
            } => {
                if param.is_const {
                    // For constants, always use numeric values
                    format!("{}", value)
                } else {
                    // Use proper namespace resolution for cross-package enum references
                    match parser::lookup_decl_from_name(enum_type, crate::Namespace::AIDL) {
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
                // A `const T[]` renders as `pub const X: &[T]`, so its
                // initializer must be a slice literal (`&[...]`) — `vec![]`
                // is not a const expression and never compiled.
                let mut res = if param.is_fixed_array {
                    "[".to_owned()
                } else if param.is_const {
                    "&[".to_owned()
                } else {
                    "vec![".to_owned()
                };
                for v in v {
                    let init_str = match &v.value {
                        // AOSP `aidl_to_rust.cpp` re-emits a byte inside an array
                        // as its unsigned `u8` representation (e.g. -1 -> 255):
                        // the array's Rust element type is `u8` (i8 maps to u8 via
                        // `array_type_name`), and Rust rejects a negated literal in
                        // a `u8` array. Positive bytes are unchanged by the cast.
                        ValueType::Byte(b) => (*b as u8).to_string(),
                        _ => v.value.to_init(param.clone()),
                    };

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
            // Map/IBinder/FileDescriptor/Holder/UserDefined have no
            // value-literal form and are unreachable from parsed const
            // expressions today (callers route them through their own
            // `Default::default()` fallback first). Emit an empty string
            // rather than `unimplemented!()` so a future caller can never
            // turn this into a panic on user input.
            _ => String::new(),
        }
    }

    fn calc_expr(
        lhs: &ConstExpr,
        operator: &str,
        rhs: &ConstExpr,
        visited: &mut std::collections::HashSet<String>,
        depth: usize,
    ) -> Result<ConstExpr, ConstExprError> {
        // Thread the cycle-guard `visited` set through operand resolution so
        // a reference cycle that crosses a binary operator (e.g.
        // `const int A = B + 1; const int B = A + 1;`) is detected instead
        // of recursing until the stack overflows. Each operand gets its OWN
        // clone of the current resolution path so that sibling references to
        // a common (non-cyclic) constant are not mistaken for a cycle.
        let lhs = lhs
            .value
            .calculate_with_visited(&mut visited.clone(), depth + 1)?;
        let rhs = rhs
            .value
            .calculate_with_visited(&mut visited.clone(), depth + 1)?;

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
                // The shift amount is range-checked in u64 BEFORE narrowing to
                // u32 — `1 << 4294967296` must not truncate to a 0-bit shift
                // and slip past the guards below. A negative amount shifts in
                // the other direction (AIDL-defined, AOSP
                // `AidlBinaryConstExpression::evaluate`).
                let raw_amount = rhs.to_i64()?;
                let amount: u64 = if raw_amount < 0 {
                    is_shl = !is_shl;
                    raw_amount.unsigned_abs()
                } else {
                    raw_amount as u64
                };

                // The shift is computed in `i64` below but stored in the
                // promoted operand type. AOSP rejects a shift amount
                // `>= sizeof(T)*8` as "Constant expression computation
                // overflows"; computing in i64 and truncating to the operand
                // type instead silently miscompiles (e.g. `1 << 40` folds to
                // 0). Reject an out-of-range amount up front to match AOSP.
                let bits: u32 = match &promoted {
                    ValueType::Int64(_) | ValueType::Reference { .. } => 64,
                    // Int32 / Byte both integral-promote to `int` for the shift.
                    _ => 32,
                };
                if amount >= bits as u64 {
                    return Err(ConstExprError::new(format!(
                        "shift amount {amount} out of range for operator '{operator}' \
                         (operand width {bits} bits)"
                    )));
                }
                let rhs_value = amount as u32;

                // AOSP `OverflowGuard::operator<<`/`>>`: a negative left
                // operand never shifts, and a left shift may move bits only
                // into (not past) the sign position — the shift amount must
                // not exceed the operand's leading-zero count. `1 << 31` and
                // `1L << 63` remain legal (amount == CLZ); `2 << 31` (which
                // silently folded to 0) and `-8 >> 1` are diagnostics.
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
                    ValueType::Reference { .. } => Ok(ConstExpr::new(ValueType::Int64(value as _))),
                    _ => Err(ConstExprError::new(format!(
                        "can't apply shift operator '{}' to non-integer type: {}",
                        operator,
                        lhs.raw_expr()
                    ))),
                }
            }
            // Checked ops mirror AOSP's OverflowGuard: `i64::MAX + 1` etc. is
            // "Constant expression computation overflows.", not a silent wrap.
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

    pub fn calculate(&self) -> Result<ConstExpr, ConstExprError> {
        self.calculate_with_visited(&mut std::collections::HashSet::new(), 0)
    }

    fn calculate_with_visited(
        &self,
        visited: &mut std::collections::HashSet<String>,
        depth: usize,
    ) -> Result<ConstExpr, ConstExprError> {
        // The `visited` set only breaks *name-reference* cycles; it does
        // nothing for a deeply nested expression tree (e.g. thousands of
        // parentheses) parsed from an untrusted `.aidl` file, which would
        // otherwise recurse until the thread stack overflows and aborts
        // the compiler. Bound the structural recursion depth as well.
        if depth > MAX_EXPR_DEPTH {
            return Err(ConstExprError::new(
                "constant expression nested too deeply (exceeded recursion limit)",
            ));
        }
        match self {
            ValueType::Unary { operator, expr } => {
                let expr = expr.value.calculate_with_visited(visited, depth + 1)?;
                if operator == "-" {
                    expr.value.unary_minus()
                } else if operator == "~" {
                    expr.value.unary_not()
                } else if operator == "!" {
                    expr.value.logical_not()
                } else if matches!(expr.value, ValueType::String(_)) {
                    // Unary `+` on a string: AOSP rejects all unary operators
                    // on strings; passing the operand through would silently
                    // drop the operator.
                    Err(ConstExprError::new(
                        "can't apply a unary operator to a string",
                    ))
                } else {
                    Ok(expr)
                }
            }
            ValueType::Expr { lhs, operator, rhs } => {
                ValueType::calc_expr(lhs, operator, rhs, visited, depth)
            }
            ValueType::Array(v) => {
                let mut array = Vec::new();

                for value in v {
                    array.push(value.value.calculate_with_visited(visited, depth + 1)?);
                }

                Ok(ConstExpr::new(ValueType::Array(array)))
            }
            ValueType::Name(name) => {
                if visited.contains(name) {
                    // A reference cycle (`const int A = B; const int B = A;`)
                    // has no well-defined value; AOSP rejects it at build
                    // time. Folding to a neutral 0 here would silently bake a
                    // fabricated constant into the generated IPC code.
                    Err(ConstExprError::new(format!(
                        "circular reference detected while resolving constant '{name}'"
                    )))
                } else {
                    visited.insert(name.clone());
                    let expr = parser::name_to_const_expr(name);
                    match expr {
                        Some(expr) => expr.value.calculate_with_visited(visited, depth + 1),
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
                // Narrowing checks mirror AOSP `ValueString` (aidl_const_
                // expressions.cpp): a value outside the declared type's range
                // is a build error, not a silent two's-complement wrap —
                // `const byte A = 128;` must not become `-128`. int/long-width
                // hex literals already wrapped into the signed range at parse
                // time (`0x80000000` is Int32 == INT32_MIN), the AOSP
                // carve-out for bit patterns; byte-width bit patterns need
                // the `u8` suffix (`0xFFu8`), exactly as in AOSP.
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
                    // `u32::try_from` rejects negatives; `char::from_u32` rejects
                    // surrogates and values above U+10FFFF. Avoids the `as u32`
                    // truncation that silently wrapped out-of-range code points.
                    let raw = self.to_i64()?;
                    let ch = u32::try_from(raw)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(|| {
                            ConstExprError::new(format!("{raw} is not a valid char code point"))
                        })?;
                    Ok(Self::new(ValueType::Char(ch)))
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

    #[test]
    fn test_division_by_zero_is_error_not_panic() {
        let expr = ValueType::new_expr(ValueType::Int32(1), "/", ValueType::Int32(0));
        assert!(expr.calculate().is_err());

        let expr = ValueType::new_expr(ValueType::Int32(5), "%", ValueType::Int32(0));
        assert!(expr.calculate().is_err());
    }

    #[test]
    fn test_integer_overflow_is_diagnostic() {
        // AOSP OverflowGuard: arithmetic overflow in the promoted type is
        // "Constant expression computation overflows.", never a silent wrap.
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
        // Legal carve-outs: shift amount == CLZ(lhs) is allowed, so `1 << 31`
        // is INT32_MIN and `1L << 63` is INT64_MIN (bit patterns, AOSP-legal).
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

        // `2 << 31` silently folded to 0 before; AOSP rejects (amount > CLZ).
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
        // AOSP rejects char operands in binary const expressions; the old
        // string-concat fold produced e.g. "a1" for `'a' + 1`.
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
        // `byte A = 128` / decimal int overflow: AOSP errors with a range
        // diagnostic; hex bit patterns already wrapped at parse time.
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
        // 0.1 + 0.2 is not exactly representable in f32; converting to Double must
        // keep the full f64 value (regression: the Double arm built a Float/f32).
        let value = 0.1_f64 + 0.2_f64;
        let converted = ConstExpr::new(ValueType::Double(value))
            .convert_to(&ValueType::Double(0.0))
            .unwrap();
        assert_eq!(converted, ConstExpr::new(ValueType::Double(value)));
    }

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

    // A pathologically deep expression tree (e.g. thousands of parens in an
    // untrusted `.aidl`) must surface a diagnostic error rather than recurse
    // until the thread stack overflows and aborts the compiler.
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
