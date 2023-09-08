use std::collections::HashMap;

macro_rules! arithmetic_bit_op {
    ($lhs:expr, $op:tt, $rhs:expr, $desc:expr, $promoted:expr, $dict:expr) => {
        {
            match $promoted {
                ValueType::Bool(_) => {
                    let value = ($lhs.to_i64($dict) $op $rhs.to_i64($dict)) != 0;
                    ConstExpr::new(ValueType::Bool(value))
                }
                ValueType::Int8(_) => {
                    let value = ($lhs.to_i64($dict) $op $rhs.to_i64($dict));
                    ConstExpr::new(ValueType::Int8(value as _))
                }
                ValueType::Int32(_) => {
                    let value = ($lhs.to_i64($dict) $op $rhs.to_i64($dict));
                    ConstExpr::new(ValueType::Int32(value as _))
                }
                ValueType::Int64(_) => {
                    let value = ($lhs.to_i64($dict) $op $rhs.to_i64($dict));
                    ConstExpr::new(ValueType::Int64(value as _))
                }
                // ValueType::Float(_) | ValueType::Double => {
                //     ConstExpr::new_with_int($lhs.to_i64($dict) $op $rhs.to_i64($dict), $promoted)
                // }
                _ => panic!("Can't apply operator {:?} - '{}' for non integer type: {} {:?} - {}",
                    $lhs, $lhs.raw_expr(), $desc, $rhs, $rhs.raw_expr()),
            }
        }
    }
}

macro_rules! arithmetic_basic_op {
    ($lhs:expr, $op:tt, $rhs:expr, $desc:expr, $promoted:expr, $dict:expr) => {
        {
            let lhs = $lhs.convert_to($promoted, $dict);
            let rhs = $rhs.convert_to($promoted, $dict);
            let as_str = &format!("{} {} {}", lhs.raw_expr(), $desc, rhs.raw_expr());

            match $promoted {
                ValueType::Void => ConstExpr::default(),
                ValueType::String(_) | ValueType::Char(_) => {
                    let value = format!("{}{}", lhs.to_string(), rhs.to_string());
                    ConstExpr::new(ValueType::String(value))
                }
                ValueType::Int8(_) => {
                    ConstExpr::new(ValueType::Int8((lhs.to_i64($dict) $op rhs.to_i64($dict)) as i8 as _))
                }
                ValueType::Int32(_) => {
                    ConstExpr::new(ValueType::Int32((lhs.to_i64($dict) $op rhs.to_i64($dict)) as i32 as _))
                }
                ValueType::Int64(_) => {
                    ConstExpr::new(ValueType::Int64((lhs.to_i64($dict) $op rhs.to_i64($dict)) as _))
                }
                ValueType::Float(_) => {
                    ConstExpr::new(ValueType::Float((lhs.to_f64($dict) $op rhs.to_f64($dict)) as f32 as _))
                }
                ValueType::Double(_) => {
                    ConstExpr::new(ValueType::Double((lhs.to_f64($dict) $op rhs.to_f64($dict)) as _))
                }
                ValueType::Bool(_) => {
                    ConstExpr::new(ValueType::Bool((lhs.to_i64($dict) $op rhs.to_i64($dict)) != 0))
                }
                _ => {
                    panic!("Can't apply operator '{}' to non integer or float type: {}", $desc, as_str);
                }
            }
        }
    }
}

#[derive(Default, Debug, Clone)]
pub enum ValueType {
    #[default] Void,
    Name(String),
    Bool(bool),
    Int8(i8),
    Int32(i32),
    Int64(i64),
    Array(Vec<ConstExpr>),
    Char(char),
    String(String),
    Float(f64),
    Double(f64),
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
    UserDefined,
}

impl ValueType {
    fn new_expr(lhs: ValueType, operator: &str, rhs: ValueType) -> ValueType {
        ValueType::Expr{
            lhs: Box::new(ConstExpr::new(lhs)),
            operator: operator.into(),
            rhs: Box::new(ConstExpr::new(rhs)),
        }
    }

    fn new_unary(operator: &str, expr: ValueType) -> ValueType {
        ValueType::Unary{
            operator: operator.into(),
            expr: Box::new(ConstExpr::new(expr)),
        }
    }

    fn order(&self) -> u32 {
        match self {
            ValueType::Void => 0,
            ValueType::Name(_) => 1,
            ValueType::Bool(_) => 2,
            ValueType::Int8(_) => 3,
            ValueType::Int32(_) => 4,
            ValueType::Int64(_) => 5,
            ValueType::Array(_) => 6,
            ValueType::Char(_) => 7,
            ValueType::String(_) => 8,
            ValueType::Float(_) => 9,
            ValueType::Double(_) => 10,
            ValueType::Expr{..} => 11,
            ValueType::Unary{..} => 12,
            ValueType::IBinder => 13,
            ValueType::FileDescriptor => 14,
            ValueType::Holder => 15,
            ValueType::UserDefined => 16,
        }
    }

    fn unary_not(&self, dict: Option<&HashMap<String, ConstExpr>>) -> ConstExpr {
        match self {
            ValueType::Void | ValueType::String(_) |
            ValueType::Char(_) => ConstExpr::new(self.clone()),
            ValueType::Int8(v) => ConstExpr::new(ValueType::Int8(!*v)),
            ValueType::Int32(v) => ConstExpr::new(ValueType::Int32(!*v)),
            ValueType::Int64(v) => ConstExpr::new(ValueType::Int64(!*v)),
            ValueType::Bool(v) => ConstExpr::new(ValueType::Bool(!*v)),
            ValueType::Expr{..} | ValueType::Unary{..} => {
                let expr = self.calculate(dict);
                expr.value.unary_not(dict)
            }
            ValueType::Array(v) => {
                let mut list = Vec::new();
                for expr in v {
                    list.push(expr.value.unary_not(dict))
                }
                ConstExpr::new(ValueType::Array(list))
            }
            _ => panic!("Can't apply unary operator '~' or \"!\" to {:?}", self),
        }
    }

    fn unary_minus(&self, dict: Option<&HashMap<String, ConstExpr>>) -> ConstExpr {
        match self {
            ValueType::Void | ValueType::String(_) | ValueType::Bool(_) |
            ValueType::Char(_) => ConstExpr::new(self.clone()),
            ValueType::Int8(v) => ConstExpr::new(ValueType::Int8((*v as i8).wrapping_neg() as _)),
            ValueType::Int32(v) => ConstExpr::new(ValueType::Int32((*v as i32).wrapping_neg() as _)),
            ValueType::Int64(v) => ConstExpr::new(ValueType::Int64(v.wrapping_neg())),
            ValueType::Float(v) => ConstExpr::new(ValueType::Float(-(*v as f32) as _)),
            ValueType::Double(v) => ConstExpr::new(ValueType::Double(-*v)),
            ValueType::Expr{..} | ValueType::Unary{..} => {
                let expr = self.calculate(dict);
                expr.value.unary_minus(dict)
            }

            ValueType::Array(v) => {
                let mut list = Vec::new();
                for expr in v {
                    list.push(expr.value.unary_minus(dict))
                }

                ConstExpr::new(ValueType::Array(list))
            }
            _ => panic!("Can't apply unary operator '-' to {:?}", self),
        }
    }

    pub fn to_bool(&self, dict: Option<&HashMap<String, ConstExpr>>) -> bool {
        match self {
            ValueType::Void => false,
            ValueType::String(_) => unimplemented!(),
            ValueType::Bool(v) => *v,
            ValueType::Char(v) => true,
            ValueType::Int8(v) => *v != 0,
            ValueType::Int32(v) => *v != 0,
            ValueType::Int64(v) => *v != 0,
            ValueType::Float(v) | ValueType::Double(v) => *v != 0.,
            ValueType::Array(_) => {
                panic!("to_bool() for List is not supported.");
            }
            ValueType::Name(_) => {
                panic!("to_bool() for Name is not supported.");
            }
            ValueType::Expr{ .. } | ValueType::Unary { .. } => {
                let expr = self.calculate(dict);
                expr.to_bool(dict)
            }
            _ => unimplemented!(),
        }
    }

    pub fn to_f64(&self, dict: Option<&HashMap<String, ConstExpr>>) -> f64 {
        match self {
            ValueType::Void => 0.,
            ValueType::String(_) => unimplemented!(),
            ValueType::Bool(v) => if *v == true { 1.0 } else { 0.0 },
            ValueType::Char(v) => *v as i64 as _,
            ValueType::Int8(v) => *v as _,
            ValueType::Int32(v) => *v as _,
            ValueType::Int64(v) => *v as _,
            ValueType::Float(v) | ValueType::Double(v) => *v as _,
            ValueType::Array(_) => {
                panic!("to_f64() for List is not supported.");
            }
            ValueType::Name(_) => {
                panic!("to_f64() for Name is not supported.");
            }
            ValueType::Expr{ .. } | ValueType::Unary{ .. } => {
                let expr = self.calculate(dict);
                expr.to_f64(dict)
            }
            _ => unimplemented!(),
        }
    }

    pub fn to_i64(&self, dict: Option<&HashMap<String, ConstExpr>>) -> i64 {
        match self {
            ValueType::Void => 0,
            ValueType::String(_) => unimplemented!(),
            ValueType::Bool(v) => *v as _,
            ValueType::Char(v) => *v as _,
            ValueType::Int8(v) => *v as _,
            ValueType::Int32(v) => *v as _,
            ValueType::Int64(v) => *v as _,
            ValueType::Float(v) | ValueType::Double(v) => *v as _,
            ValueType::Array(_) => {
                panic!("to_i64() for List is not supported. {:?}", self);
            }
            ValueType::Name(_) => {
                panic!("to_i64() for Name is not supported.");
            }
            ValueType::Expr{ .. } | ValueType::Unary{ .. } => {
                let expr = self.calculate(dict);
                expr.to_i64(dict)
            }
            _ => unimplemented!(),
        }
    }

    fn char_to_string(ch: char) -> String {
        match ch {
            '\\' => String::from("\\\\"),
            '\'' => String::from("\\'"),
            '\"' => String::from("\\\""),
            '\n' => String::from("\\n"),
            '\t' => String::from("\\t"),
            // ... 다른 특수 문자들을 필요에 따라 추가 ...
            _ => ch.to_string(),
        }
    }

    pub fn to_init(&self, is_const: bool) -> String {
        match self {
            ValueType::String(_) => {
                if is_const == true {
                    format!("\"{}\"", self.to_string())
                } else {
                    format!("\"{}\".into()", self.to_string())
                }
            }
            ValueType::Float(_) => format!("{}f32", self.to_string()),
            ValueType::Double(_) => format!("{}f64", self.to_string()),
            ValueType::Char(_) => format!("'{}'", self.to_string()),
            ValueType::Int8(_) | ValueType::Int32(_) | ValueType::Int64(_) |
            ValueType::Bool(_) | ValueType::Array(_) |
            ValueType::Expr{ .. } | ValueType::Unary{ .. } |
            ValueType::Name(_) => self.to_string(),
            _ => format!("Default::default()"),
        }
    }

    pub fn to_string(&self) -> String {
        match self {
            ValueType::Void => "".into(),
            ValueType::String(v) => v.clone(),
            ValueType::Int8(v) => v.to_string(),
            ValueType::Int32(v) => v.to_string(),
            ValueType::Int64(v) => v.to_string(),
            ValueType::Float(v) => (*v as f32).to_string(),
            ValueType::Double(v) => (*v as f64).to_string(),
            ValueType::Bool(v) => v.to_string(),
            ValueType::Char(v) => Self::char_to_string(*v),
            ValueType::Array(v) => {
                let mut res = "vec![".to_owned();
                for v in v {
                    res += &(v.to_string() + ",");
                }

                res += "]";

                res
            }
            ValueType::Name(v) => {
                format!("{{{}}}", v)
            }
            ValueType::Expr{ lhs, operator, rhs} => {
                format!("{} {} {}", lhs.to_string(), operator, rhs.to_string())
            }
            ValueType::Unary{ operator, expr } => {
                format!("{} {}", operator, expr.to_string())
            }
            _ => unimplemented!(),
        }
    }

    fn calc_expr(lhs: &Box<ConstExpr>, operator: &str, rhs: &Box<ConstExpr>,
        dict: Option<&HashMap<String, ConstExpr>>) -> ConstExpr {
        let lhs = lhs.calculate(dict);
        let rhs = rhs.calculate(dict);

        let promoted = type_conversion(integral_promotion(lhs.value.clone()), integral_promotion(rhs.value.clone()));

        match operator {
            "||" => {
                ConstExpr::new(ValueType::Bool(lhs.to_bool(dict) || rhs.to_bool(dict)))
            }
            "&&" => {
                ConstExpr::new(ValueType::Bool(lhs.to_bool(dict) && rhs.to_bool(dict)))
            }
            "|" => {
                arithmetic_bit_op!(lhs, |, rhs, "|", promoted, dict)
            }
            "^" => {
                arithmetic_bit_op!(lhs, ^, rhs, "^", promoted, dict)
            }
            "&" => {
                arithmetic_bit_op!(lhs, &, rhs, "&", promoted, dict)
            }
            "==" | "!=" | "<=" | ">=" | "<" | ">" => {
                let lhs = lhs.convert_to(&promoted, dict);
                let rhs = rhs.convert_to(&promoted, dict);

                let value = match operator {
                    "==" => lhs == rhs,
                    "!=" => lhs != rhs,
                    "<=" => lhs <= rhs,
                    ">=" => lhs >= rhs,
                    "<" => lhs < rhs,
                    ">" => lhs > rhs,
                    _ => unreachable!(),
                };

                ConstExpr::new(ValueType::Bool(value))
            }

            "<<" | ">>" => {
                let mut is_shl = if operator == "<<" {
                    true
                } else {
                    false
                };

                let lhs_value = lhs.to_i64(dict);
                let rhs_value = rhs.to_i64(dict);
                let rhs_value: u32 = if rhs_value < 0 {
                    is_shl = !is_shl;
                    rhs_value.wrapping_neg() as _
                } else {
                    rhs_value as _
                };

                let value = if is_shl == true {
                    lhs_value.wrapping_shl(rhs_value)
                } else {
                    lhs_value.wrapping_shr(rhs_value)
                };

                match promoted {
                    ValueType::Int32(_) => ConstExpr::new(ValueType::Int32(value as _)),
                    ValueType::Int64(_) => ConstExpr::new(ValueType::Int64(value as _)),
                    ValueType::Int8(_) => ConstExpr::new(ValueType::Int8(value as _)),
                    _ => panic!("Can't apply operator '{}' for non integer type: {}", operator, lhs.raw_expr()),
                }
            }
            "+" => arithmetic_basic_op!(lhs, +, rhs, "+", &promoted, dict),
            "-" => arithmetic_basic_op!(lhs, -, rhs, "-", &promoted, dict),
            "*" => arithmetic_basic_op!(lhs, *, rhs, "*", &promoted, dict),
            "/" => arithmetic_basic_op!(lhs, /, rhs, "/", &promoted, dict),
            "%" => arithmetic_basic_op!(lhs, %, rhs, "%", &promoted, dict),
            _ => unreachable!(),
        }
    }

    pub fn calculate(&self, dict: Option<&HashMap<String, ConstExpr>>) -> ConstExpr {
        match self {
            ValueType::Unary{ operator, expr } => {
                let expr = expr.calculate(dict);
                if operator == "-" {
                    expr.value.unary_minus(dict)
                } else if operator == "~" || operator == "!" {
                    expr.value.unary_not(dict)
                } else {
                    expr
                }
            }
            ValueType::Expr{ lhs, operator, rhs } => {
                ValueType::calc_expr(lhs, operator, rhs, dict)
            }
            ValueType::Array(v) => {
                let mut array = Vec::new();

                for value in v {
                    array.push(value.calculate(dict));
                }

                ConstExpr::new(ValueType::Array(array))
            }
            ValueType::Name(name) => {
                if let Some(dict) = dict {
                    if let Some(expr) = dict.get(name) {
                        expr.calculate(Some(dict)).clone()
                    } else {
                        panic!("Can't find {} from Dictionary.", name);
                    }
                } else {
                    panic!("ValueType is Name. But, there is no dictionary to find the Name.");
                }
            }
            _ => ConstExpr::new(self.clone()),
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
            ValueType::Void => if let ValueType::Void = rhs {
                Some(std::cmp::Ordering::Equal)
            } else {
                Some(std::cmp::Ordering::Less)
            },
            ValueType::String(v) | ValueType::Name(v) => v.partial_cmp(&rhs.to_string()),
            ValueType::Int8(v) => v.partial_cmp(&(rhs.to_i64(None) as _)),
            ValueType::Int32(v) => v.partial_cmp(&(rhs.to_i64(None) as _)),
            ValueType::Int64(v) => v.partial_cmp(&(rhs.to_i64(None) as _)),
            ValueType::Char(v) => (*v as i64).partial_cmp(&rhs.to_i64(None)),
            ValueType::Bool(v) => v.partial_cmp(&rhs.to_bool(None)),
            ValueType::Float(v) | ValueType::Double(v) => v.partial_cmp(&rhs.to_f64(None)),
            ValueType::Array(lhs_array) => {
                if let ValueType::Array(rhs_array) = rhs {
                    lhs_array.partial_cmp(rhs_array)
                } else {
                    None
                }
            }
            ValueType::Unary { .. } | ValueType::Expr{..} => {
                let lhs = self.calculate(None);
                let rhs = rhs.calculate(None);
                lhs.partial_cmp(&rhs)
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

    pub value: ValueType,
}


impl ConstExpr {
    pub fn new(value: ValueType) -> Self {
        Self {
            value,
            .. Default::default()
        }
    }

    pub fn new_expr(lhs: ConstExpr, operator: &str, rhs: ConstExpr) -> Self {
        Self {
            value: ValueType::Expr {
                lhs: Box::new(lhs),
                operator: operator.into(),
                rhs: Box::new(rhs),
            },
            .. Default::default()
        }
    }

    pub fn new_unary(operator: &str, expr: ConstExpr) -> Self {
        Self {
            value: ValueType::Unary {
                operator: operator.into(),
                expr: Box::new(expr),
            },
            .. Default::default()
        }
    }

    pub fn set_raw_expr(&mut self, raw_expr: &str) {
        self.raw_expr = raw_expr.into();
    }

    pub fn raw_expr(&self) -> &str {
        &self.raw_expr
    }

    pub fn to_string(&self) -> String {
        self.value.to_string()
    }

    pub fn to_i64(&self, dict: Option<&HashMap<String, ConstExpr>>) -> i64 {
        self.value.to_i64(dict)
    }

    pub fn to_f64(&self, dict: Option<&HashMap<String, ConstExpr>>) -> f64 {
        self.value.to_f64(dict)
    }

    pub fn to_bool(&self, dict: Option<&HashMap<String, ConstExpr>>) -> bool {
        self.value.to_bool(dict)
    }

    pub fn convert_to(&self, value_type: &ValueType, dict: Option<&HashMap<String, ConstExpr>>) -> ConstExpr {
        if self.value.order() == value_type.order() {
            self.clone()
        } else {
            if let ValueType::Array(list) = &self.value {
                let mut res = Vec::new();

                for v in list {
                    res.push(v.convert_to(value_type, dict))
                }
                ConstExpr::new(ValueType::Array(res))
            } else {
                match value_type {
                    ValueType::Void => Self::default(),
                    ValueType::String(_) => ConstExpr::new(ValueType::String(self.to_string())),
                    ValueType::Int8(_) => ConstExpr::new(ValueType::Int8(self.to_i64(dict) as i8 as _)),
                    ValueType::Int32(_) => ConstExpr::new(ValueType::Int32(self.to_i64(dict) as i32 as _)),
                    ValueType::Int64(_) => ConstExpr::new(ValueType::Int64(self.to_i64(dict))),
                    ValueType::Float(_) => ConstExpr::new(ValueType::Float(self.to_f64(dict) as f32 as _)),
                    ValueType::Double(_) => ConstExpr::new(ValueType::Float(self.to_f64(dict))),
                    ValueType::Bool(_) => ConstExpr::new(ValueType::Bool(self.to_bool(dict))),
                    ValueType::Char(_) => {
                        let ch = self.to_i64(dict) as u32;
                        if let Some(ch) = char::from_u32(ch) {
                            Self::new(ValueType::Char(ch as _))
                        } else {
                            panic!("0x{:x} is invalid unicode.", ch)
                        }
                    }
                    ValueType::Array(_) => {
                        unimplemented!();
                        // let mut res = Vec::new();
                        // for v in &v {
                        //     res.push(v.convert_to(value_type, dict));
                        // }

                        // Self::new_with_array(res)
                    }
                    ValueType::Name(_) => {
                        unreachable!();
                    }
                    ValueType::Expr {..} | ValueType::Unary {..} => {
                        unreachable!();
                    }
                    _ => unimplemented!(),

                }
            }
        }
    }

    pub fn calculate(&self, dict: Option<&HashMap<String, ConstExpr>>) -> ConstExpr {
        self.value.calculate(dict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expression_arithmatic() {
        let expr = ValueType::new_expr(ValueType::Int32(10), "+", ValueType::Int32(10));

        assert_eq!(expr.calculate(None), ConstExpr::new(ValueType::Int32(20)));

        let expr = ValueType::new_expr(
            ValueType::Int8(1),
            "<<",
            ValueType::Int8(31),
        );

        assert_eq!(expr.calculate(None), ConstExpr::new(ValueType::Int32(0x80000000u32 as _)));

        // assert_eq!(expr.calculate(&mut dict), Expression::Int32(100));

        let expr = ValueType::new_expr(
            ValueType::Int8(10),
            "/",
            ValueType::Float(2.0),
        );

        assert_eq!(expr.calculate(None), ConstExpr::new(ValueType::Float(5.0)));


        let expr = ValueType::new_expr(
            ValueType::Float(10.0),
            "%",
            ValueType::Float(2.0),
        );

        assert_eq!(expr.calculate(None), ConstExpr::new(ValueType::Float(10.0 % 2.0)));

        let expr = ValueType::new_expr(
            ValueType::Int32(10),
            "%",
            ValueType::Bool(true),
        );

        assert_eq!(expr.calculate(None), ConstExpr::new(ValueType::Int32(10 % 1)));
    }

    #[test]
    fn test_const_expr_name() {
        let mut dict = HashMap::new();
        dict.insert("DUMP_FLAG_PRIORITY_CRITICAL".to_owned(),
            ConstExpr::new(
                ValueType::new_expr(
                    ValueType::Int32(1),
                    "<<",
                    ValueType::Int32(0),
                )
            )
        );

        dict.insert("DUMP_FLAG_PRIORITY_HIGH".to_owned(),
            ConstExpr::new(
                ValueType::new_expr(
                    ValueType::Int32(1),
                    "<<",
                    ValueType::Int32(1),
                )
            )
        );

        dict.insert("DUMP_FLAG_PRIORITY_NORMAL".to_owned(),
            ConstExpr::new(
                ValueType::new_expr(
                    ValueType::Int32(1),
                    "<<",
                    ValueType::Int32(2),
                )
            )
        );

        dict.insert("DUMP_FLAG_PRIORITY_DEFAULT".to_owned(),
            ConstExpr::new(
                ValueType::new_expr(
                    ValueType::Int32(1),
                    "<<",
                    ValueType::Int32(3),
                )
            )
        );

        let expr = ConstExpr::new(
            ValueType::new_expr(
                ValueType::Name("DUMP_FLAG_PRIORITY_CRITICAL".into()),
                "|",
                ValueType::new_expr(
                    ValueType::Name("DUMP_FLAG_PRIORITY_HIGH".into()),
                    "|",
                    ValueType::new_expr(
                        ValueType::Name("DUMP_FLAG_PRIORITY_NORMAL".into()),
                        "|",
                        ValueType::Name("DUMP_FLAG_PRIORITY_DEFAULT".into()),
                    )
                ),
            )
        );

        assert_eq!(expr.calculate(Some(&dict)), ConstExpr::new(ValueType::Int32(15)));
    }
}