use std::collections::HashMap;

macro_rules! arithmetic_bit_op {
    ($lhs:expr, $op:tt, $rhs:expr, $desc:expr, $promoted:expr, $dict:expr) => {
        {
            // let as_str = format!("{} {} {}", $lhs.raw_expr(), &$desc, $rhs.raw_expr());

            match $promoted {
                ValueType::Bool => {
                    let value = ($lhs.int_value $op $rhs.int_value) != 0;
                    ConstExpr::new_with_int(value as i64, $promoted)
                }
                ValueType::Int8 | ValueType::Int32 | ValueType::Int64 => {
                    ConstExpr::new_with_int($lhs.int_value $op $rhs.int_value, $promoted)
                }
                ValueType::Float | ValueType::Double => {
                    ConstExpr::new_with_int($lhs.to_i64($dict) $op $rhs.to_i64($dict), $promoted)
                }
                _ => panic!("Can't apply operator '{}' for non integer type: {} {}", $lhs.raw_expr(), $desc, $rhs.raw_expr()),
            }
        }
    }
}

macro_rules! arithmetic_basic_op {
    ($lhs:expr, $op:tt, $rhs:expr, $desc:expr, $promoted:expr) => {
        {
            let lhs = $lhs.convert_to($promoted);
            let rhs = $rhs.convert_to($promoted);
            let as_str = &format!("{} {} {}", lhs.raw_expr(), $desc, rhs.raw_expr());

            match $promoted {
                ValueType::Void => ConstExpr::default(),
                ValueType::String | ValueType::Char => {
                    let value = format!("{}{}", lhs.to_string(), rhs.to_string());
                    ConstExpr::new_with_str(&value, $promoted)
                }
                ValueType::Int8 | ValueType::Int32 | ValueType::Int64 => {
                    ConstExpr::new_with_int(lhs.int_value $op rhs.int_value, $promoted)
                }
                ValueType::Float | ValueType::Double => {
                    ConstExpr::new_with_float(lhs.float_value $op rhs.float_value, $promoted)
                }
                ValueType::Bool => {
                    let value = (lhs.int_value $op rhs.int_value) != 0;
                    ConstExpr::new_with_int(value as i64, $promoted)
                }
                _ => {
                    panic!("Can't apply operator '{}' to non integer or float type: {}", $desc, as_str);
                }
            }
        }
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum ValueType {
    #[default] Void = 0,
    Name = 1,
    Bool = 2,
    Int8 = 3,
    Int32 = 4,
    Int64 = 5,
    Array = 6,
    Char = 7,
    String = 8,
    Float = 9,
    Double = 10,
    Expr = 11,
    IBinder = 12,
    FileDescriptor = 13,
    Holder = 14,
    List = 15,
    UserDefined = 16,
}

fn type_conversion(lhs: ValueType, rhs: ValueType) -> ValueType {
    if lhs == rhs {
        lhs
    } else if lhs == ValueType::Bool {
        rhs
    } else if rhs == ValueType::Bool {
        lhs
    } else if lhs > rhs {
        lhs
    } else {
        rhs
    }
}

fn integral_promotion(value_type: ValueType) -> ValueType {
    if value_type > ValueType::Int32 {
        value_type
    } else {
        ValueType::Int32
    }
}

#[derive(Debug, Clone)]
pub struct Expression {
    lhs: Box<ConstExpr>,
    operator: String,
    rhs: Box<ConstExpr>,

    is_unary: bool,
}

impl Expression {
    pub fn new(lhs: ConstExpr, operator: &str, rhs: ConstExpr) -> Self {
        Self {
            lhs: Box::new(lhs),
            operator: operator.into(),
            rhs: Box::new(rhs),
            is_unary: false,
        }
    }

    pub fn new_with_unary(operator: &str, rhs: ConstExpr) -> Self {
        Self {
            lhs: Box::new(ConstExpr::default()),
            operator: operator.into(),
            rhs: Box::new(rhs),
            is_unary: true,
        }
    }

    pub fn to_string(&self) -> String {
        format!("{}{}{}", self.lhs.to_string(), self.operator, self.rhs.to_string())
    }

    pub fn calculate(&self, dict: Option<&HashMap<String, ConstExpr>>) -> ConstExpr {
        if self.is_unary == true {
            let mut expr = self.rhs.calculate(dict);
            if self.operator == "-" {
                expr = expr.unary_minus();
            } else if self.operator == "~" || self.operator == "!" {
                expr = expr.unary_not();
            }

            expr
        } else {
            let lhs = self.lhs.calculate(dict);
            let rhs = self.rhs.calculate(dict);

            let promoted = type_conversion(integral_promotion(lhs.value_type), integral_promotion(rhs.value_type));

            match self.operator.as_str() {
                "||" => {
                    ConstExpr::new_with_int((lhs.to_bool(dict) || rhs.to_bool(dict)) as i64, ValueType::Bool)
                }
                "&&" => {
                    ConstExpr::new_with_int((lhs.to_bool(dict) && rhs.to_bool(dict)) as i64, ValueType::Bool)
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
                    let lhs = lhs.convert_to(promoted);
                    let rhs = rhs.convert_to(promoted);
                    let value = match self.operator.as_str() {
                        "==" => lhs == rhs,
                        "!=" => lhs != rhs,
                        "<=" => lhs <= rhs,
                        ">=" => lhs >= rhs,
                        "<" => lhs < rhs,
                        ">" => lhs > rhs,
                        _ => unreachable!(),
                    };

                    ConstExpr::new_with_int(value as i64, ValueType::Bool)
                }

                "<<" | ">>" => {
                    let mut is_shl = if self.operator == "<<" {
                        true
                    } else {
                        false
                    };

                    let rhs_value = rhs.to_i64(dict);
                    let rhs_value: u32 = if rhs_value < 0 {
                        is_shl = !is_shl;
                        rhs_value.wrapping_neg() as u32
                    } else {
                        rhs_value as u32
                    };

                    let value = if is_shl == true {
                        lhs.int_value.wrapping_shl(rhs_value)
                    } else {
                        lhs.int_value.wrapping_shr(rhs_value)
                    };

                    match promoted {
                        ValueType::Int32 | ValueType::Int64 | ValueType::Int8 | ValueType::Bool => {
                            ConstExpr::new_with_int(value, promoted)
                        }
                        _ => panic!("Can't apply operator '{}' for non integer type: {}", self.operator, lhs.raw_expr()),
                    }
                }
                "+" => arithmetic_basic_op!(lhs, +, rhs, "+", promoted),
                "-" => arithmetic_basic_op!(lhs, -, rhs, "-", promoted),
                "*" => arithmetic_basic_op!(lhs, *, rhs, "*", promoted),
                "/" => arithmetic_basic_op!(lhs, /, rhs, "/", promoted),
                "%" => arithmetic_basic_op!(lhs, %, rhs, "%", promoted),
                _ => unreachable!(),
            }
        }
    }
}

impl PartialEq for ConstExpr {
    fn eq(&self, rhs: &Self) -> bool {
        self.partial_cmp(rhs) == Some(std::cmp::Ordering::Equal)
    }
}

impl PartialOrd for ConstExpr {
    fn partial_cmp(&self, rhs: &Self) -> Option<std::cmp::Ordering> {
        match self.value_type {
            ValueType::Void => Some(std::cmp::Ordering::Equal),
            ValueType::String | ValueType::Name => self.str_value.partial_cmp(&rhs.str_value),
            ValueType::Int8 | ValueType::Int32 | ValueType::Int64 | ValueType::Char |
            ValueType::Bool => self.int_value.partial_cmp(&rhs.int_value),
            ValueType::Float | ValueType::Double => self.float_value.partial_cmp(&rhs.float_value),
            _ => None,
        }
    }
}


#[derive(Debug, Clone, Default)]
pub struct ConstExpr {
    pub raw_expr: String,

    pub int_value: i64,
    pub str_value: String,
    pub float_value: f64,
    pub array_value: Vec<ConstExpr>,
    pub expr_value: Option<Expression>,

    pub value_type: ValueType,
}

// impl Default for ConstExpr {
//     fn default() -> Self {
//         Self {
//             value_type: ValueType::Void,
//             expr_value: None,
//             array_value: Vec::new(),
//             .. Default::default()
//         }
//     }
// }

impl ConstExpr {
    pub fn new_with_int(value: i64, value_type: ValueType) -> Self {
        Self {
            int_value: value,
            value_type,
            .. Default::default()
        }
    }

    pub fn new_with_float(value: f64, value_type: ValueType) -> Self {
        Self {
            float_value: value,
            value_type,
            .. Default::default()
        }
    }

    pub fn new_with_str(value: &str, value_type: ValueType) -> Self {
        Self {
            str_value: value.into(),
            value_type,
            .. Default::default()
        }
    }

    pub fn new_with_array(value: Vec<ConstExpr>) -> Self {
        Self {
            array_value: value,
            value_type: ValueType::Array,
            .. Default::default()
        }
    }

    pub fn new_with_expr(value: Expression) -> Self {
        Self {
            expr_value: Some(value),
            value_type: ValueType::Expr,
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
        match self.value_type {
            ValueType::Void => "".into(),
            ValueType::String => self.str_value.clone(),
            ValueType::Int8 => (self.int_value as i8).to_string(),
            ValueType::Int32 => (self.int_value as i32).to_string(),
            ValueType::Int64 => (self.int_value as i64).to_string(),
            ValueType::Float => (self.int_value as f32).to_string(),
            ValueType::Double => (self.int_value as f64).to_string(),
            ValueType::Bool => (self.int_value != 0).to_string(),
            ValueType::Char => {
                if let Some(ch) = char::from_u32(self.int_value as u32) {
                    ch.to_string()
                } else {
                    panic!("0x{:x} is invalid unicode.", self.int_value)
                }
            }
            ValueType::Array => {
                let mut res = "vec![".to_owned();
                for v in &self.array_value {
                    res += &(v.to_string() + ",");
                }

                res += "]";

                res
            }
            ValueType::Name => {
                format!("{{{}}}", self.str_value)
            }
            ValueType::Expr => {
                self.expr_value.as_ref().map_or(String::new(), |expr| expr.to_string())
            }
            _ => unimplemented!(),
        }
    }

    pub fn to_i64(&self, dict: Option<&HashMap<String, ConstExpr>>) -> i64 {
        match self.value_type {
            ValueType::Void => 0,
            ValueType::String => unimplemented!(),
            ValueType::Bool | ValueType::Char | ValueType::Int8 |
            ValueType::Int32 | ValueType::Int64 => self.int_value,
            ValueType::Float | ValueType::Double => self.float_value as i64,
            ValueType::Array => {
                panic!("to_i64() for List is not supported.");
            }
            ValueType::Name => {
                panic!("to_i64() for Name is not supported.");
            }
            ValueType::Expr => {
                let expr = self.expr_value.as_ref().map_or(ConstExpr::default(), |expr| expr.calculate(dict));
                expr.to_i64(dict)
            }
            _ => unimplemented!(),
        }
    }

    pub fn to_bool(&self, dict: Option<&HashMap<String, ConstExpr>>) -> bool {
        match self.value_type {
            ValueType::Void => false,
            ValueType::String => unimplemented!(),
            ValueType::Bool | ValueType::Char | ValueType::Int8 |
            ValueType::Int32 | ValueType::Int64 => self.int_value != 0,
            ValueType::Float | ValueType::Double => self.float_value != 0.,
            ValueType::Array => {
                panic!("to_bool() for List is not supported.");
            }
            ValueType::Name => {
                panic!("to_bool() for Name is not supported.");
            }
            ValueType::Expr => {
                let expr = self.expr_value.as_ref().map_or(ConstExpr::default(), |expr| expr.calculate(dict));
                expr.to_bool(dict)
            }
            _ => unimplemented!(),
        }
    }

    pub fn convert_to(&self, value_type: ValueType) -> ConstExpr {
        if self.value_type == value_type {
            self.clone()
        } else {
            match value_type {
                ValueType::Void => Self::default(),
                ValueType::String => Self::new_with_str(&self.to_string(), value_type),
                ValueType::Int8 => Self::new_with_int(self.int_value as i8 as i64, value_type),
                ValueType::Int32 => Self::new_with_int(self.int_value as i32 as i64, value_type),
                ValueType::Int64 => Self::new_with_int(self.int_value, value_type),
                ValueType::Float => Self::new_with_float(self.float_value as f32 as f64, value_type),
                ValueType::Double => Self::new_with_float(self.float_value, value_type),
                ValueType::Bool => Self::new_with_int(self.int_value, value_type),
                ValueType::Char => {
                    if let Some(ch) = char::from_u32(self.int_value as u32) {
                        Self::new_with_int(ch as i64, value_type)
                    } else {
                        panic!("0x{:x} is invalid unicode.", self.int_value)
                    }
                }
                ValueType::Array => {
                    let mut res = Vec::new();
                    for v in &self.array_value {
                        res.push(v.convert_to(value_type));
                    }

                    Self::new_with_array(res)
                }
                ValueType::Name => {
                    unreachable!();
                }
                ValueType::Expr => {
                    Self::new_with_expr(Expression::new_with_unary("+", self.clone()))
                }
                _ => unimplemented!(),

            }
        }
    }

    pub fn calculate(&self, dict: Option<&HashMap<String, ConstExpr>>) -> ConstExpr {
        match self.value_type {
            ValueType::Array => {
                let mut array = Vec::new();

                for value in &self.array_value {
                    array.push(value.calculate(dict));
                }

                ConstExpr::new_with_array(array)
            }
            ValueType::Expr => {
                self.expr_value.as_ref().map_or(ConstExpr::default(), |expr| expr.calculate(dict))
            }
            ValueType::Name => {
                if let Some(dict) = dict {
                    if let Some(expr) = dict.get(self.str_value.as_str()) {
                        expr.clone()
                    } else {
                        panic!("Can't find {} from Dictionary.", self.str_value);
                    }
                } else {
                    panic!("ValueType is Name. But, there is no dictionary to find the Name.");
                }
            }
            _ => self.clone(),
        }

    }

    fn unary_minus(&self) -> ConstExpr {
        let mut expr = self.clone();
        match self.value_type {
            ValueType::Void | ValueType::String | ValueType::Bool | ValueType::Char => {}
            ValueType::Int8 => expr.int_value = (self.int_value as i8).wrapping_neg() as i64,
            ValueType::Int32 => expr.int_value = (self.int_value as i32).wrapping_neg() as i64,
            ValueType::Int64 => expr.int_value = (self.int_value as i64).wrapping_neg() as i64,
            ValueType::Float => expr.float_value = (-(self.float_value as f32)) as f64,
            ValueType::Double => expr.float_value = -self.float_value,
            ValueType::Array => {
                let mut list = Vec::new();
                for value in &self.array_value {
                    list.push(value.unary_minus())
                }

                expr.array_value = list;
            }
            _ => panic!("Can't apply unary operator '-' to {:?}\n{}", expr, expr.raw_expr()),
        }

        expr
    }

    fn unary_not(&self) -> ConstExpr {
        let mut expr = self.clone();

        match self.value_type {
            ValueType::Void | ValueType::String | ValueType::Char => {}
            ValueType::Int8 | ValueType::Int32 | ValueType::Int64 => expr.int_value = !self.int_value,
            ValueType::Bool => expr.int_value = (!(self.int_value != 0)) as i64,
            ValueType::Array => {
                let mut list = Vec::new();
                for value in &self.array_value {
                    list.push(value.unary_not())
                }

                expr.array_value = list;
            }
            _ => panic!("Can't apply unary operator '~' or \"!\" to {:?}\n{}", self, self.raw_expr()),
        }

        expr
    }




    // fn to_expression(&self, arg_expr: &Expression) -> ConstExpr {
    //     match self {
    //         ConstExpr::Expression(v) => ConstExpr::Expression(v.convert_to(arg_expr)),
    //         ConstExpr::List(list) => {
    //             let mut res = Vec::new();
    //             for expr in list {
    //                 res.push(Box::new(expr.convert_to(&ConstExpr::Expression(arg_expr.clone()))))
    //             }
    //             ConstExpr::List(res)
    //         }
    //         _ => panic!("Can't convert {:?} to Expression {:?}", self, arg_expr),
    //     }
    // }

    // fn to_list(&self, arg_expr: &ConstExpr) -> ConstExpr {
    //     match self {
    //         ConstExpr::Expression(_) |
    //         ConstExpr::String(_) |
    //         ConstExpr::Char(_) => ConstExpr::List(vec![Box::new(self.convert_to(arg_expr))]),
    //         ConstExpr::List(list) => {
    //             let mut res = Vec::new();
    //             for expr in list {
    //                 res.push(Box::new(expr.convert_to(arg_expr)))
    //             }

    //             ConstExpr::List(res)
    //         }
    //     }
    // }

    // pub fn calculate(&self, dict: &mut HashMap<String, ConstExpr>) -> ConstExpr {
    //     let res = match self {
    //         ConstExpr::Char(_v) => self.clone(),
    //         ConstExpr::String(v) => {
    //             ConstExpr::String(v.calculate(dict))
    //         }
    //         ConstExpr::List(list) => {
    //             let mut res = Vec::new();
    //             for v in list {
    //                 res.push(Box::new(v.calculate(dict)))
    //             }
    //             ConstExpr::List(res)
    //         }
    //         ConstExpr::Expression(v) => {
    //             ConstExpr::Expression(v.calculate(dict))
    //         }
    //     };
    //     res
    // }
}

// impl PartialOrd for ConstExpr {
//     fn partial_cmp(&self, rhs: &Self) -> Option<std::cmp::Ordering> {
//         match self {
//             ConstExpr::Int32(lhs) => {
//                 let rhs = if let ConstExpr::Int32(rhs) = rhs {
//                     *rhs
//                 } else if let ConstExpr::IntU8(rhs) = rhs {
//                     *rhs as i64
//                 } else {
//                     return None
//                 };

//                 if lhs < &rhs {
//                     Some(std::cmp::Ordering::Less)
//                 } else if lhs == &rhs {
//                     Some(std::cmp::Ordering::Equal)
//                 } else {
//                     Some(std::cmp::Ordering::Greater)
//                 }
//             }
//             ConstExpr::Float(lhs) => {
//                 let rhs = if let ConstExpr::Float(rhs) = rhs {
//                     *rhs
//                 } else {
//                     return None
//                 };
//                 if lhs < &rhs {
//                     Some(std::cmp::Ordering::Less)
//                 } else if lhs == &rhs {
//                     Some(std::cmp::Ordering::Equal)
//                 } else {
//                     Some(std::cmp::Ordering::Greater)
//                 }
//             }
//             _ => return None
//         }
//     }
// }


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expression_arithmatic() {
        let expr = Expression::new(
            ConstExpr::new_with_int(10, ValueType::Int32),
            "+",
            ConstExpr::new_with_int(10, ValueType::Int32),
        );

        assert_eq!(expr.calculate(None), ConstExpr::new_with_int(20, ValueType::Int32));

        let expr = Expression::new(
            ConstExpr::new_with_int(1, ValueType::Int8),
            "<<",
            ConstExpr::new_with_int(31, ValueType::Int8),
        );

        assert_eq!(expr.calculate(None), ConstExpr::new_with_int(0x80000000, ValueType::Int32));

        // assert_eq!(expr.calculate(&mut dict), Expression::Int32(100));

        let expr = Expression::new(
            ConstExpr::new_with_int(10, ValueType::Int8),
            "/",
            ConstExpr::new_with_float(2.0, ValueType::Float),
        );

        assert_eq!(expr.calculate(None), ConstExpr::new_with_float(5.0, ValueType::Float));


        let expr = Expression::new(
            ConstExpr::new_with_float(10.0, ValueType::Float),
            "%",
            ConstExpr::new_with_float(2.0, ValueType::Float),
        );

        assert_eq!(expr.calculate(None), ConstExpr::new_with_float(10.0 % 2.0, ValueType::Float));

        let expr = Expression::new(
            ConstExpr::new_with_int(10, ValueType::Int32),
            "%",
            ConstExpr::new_with_int(1, ValueType::Bool),
        );

        assert_eq!(expr.calculate(None), ConstExpr::new_with_int(10 % 1, ValueType::Int32));
    }

    #[test]
    fn test_const_expr_name() {
        let mut dict = HashMap::new();
        dict.insert("DUMP_FLAG_PRIORITY_CRITICAL".to_owned(),
            ConstExpr::new_with_expr(
                Expression::new(
                    ConstExpr::new_with_int(1, ValueType::Int32),
                    "<<",
                    ConstExpr::new_with_int(0, ValueType::Int32),
                )
            )
        );

        dict.insert("DUMP_FLAG_PRIORITY_HIGH".to_owned(),
            ConstExpr::new_with_expr(
                Expression::new(
                    ConstExpr::new_with_int(1, ValueType::Int32),
                    "<<",
                    ConstExpr::new_with_int(1, ValueType::Int32),
                )
            )
        );

        dict.insert("DUMP_FLAG_PRIORITY_NORMAL".to_owned(),
            ConstExpr::new_with_expr(
                Expression::new(
                    ConstExpr::new_with_int(1, ValueType::Int32),
                    "<<",
                    ConstExpr::new_with_int(2, ValueType::Int32),
                )
            )
        );

        dict.insert("DUMP_FLAG_PRIORITY_DEFAULT".to_owned(),
            ConstExpr::new_with_expr(
                Expression::new(
                    ConstExpr::new_with_int(1, ValueType::Int32),
                    "<<",
                    ConstExpr::new_with_int(3, ValueType::Int32),
                )
            )
        );

        let expr = ConstExpr::new_with_expr(
            Expression::new(
                ConstExpr::new_with_str("DUMP_FLAG_PRIORITY_CRITICAL", ValueType::Name),
                "|",
                ConstExpr::new_with_expr(
                    Expression::new(
                        ConstExpr::new_with_str("DUMP_FLAG_PRIORITY_HIGH", ValueType::Name),
                        "|",
                        ConstExpr::new_with_expr(
                            Expression::new(
                                ConstExpr::new_with_str("DUMP_FLAG_PRIORITY_NORMAL", ValueType::Name),
                                "|",
                                ConstExpr::new_with_str("DUMP_FLAG_PRIORITY_DEFAULT", ValueType::Name),
                            )
                        )
                    )
                ),
            )
        );

        assert_eq!(expr.calculate(Some(&dict)), ConstExpr::new_with_int(15, ValueType::Int32));
    }
}