use std::collections::HashMap;
use std::fmt;

fn bool_to_int(v: bool) -> i32 {
    if v == true { 1 } else { 0 }
}

macro_rules! arithmetic_op {
    ($lhs:expr, $op:tt, $rhs:expr, $desc:expr, $as_str:expr) => {
        match $lhs {
            Expression::Int(lhs) => {
                if let Expression::Int(rhs) = $rhs {
                    Expression::Int(lhs $op rhs)
                } else if let Expression::Long(rhs) = $rhs {
                    Expression::Int(lhs $op (rhs as i32))
                } else if let Expression::Int8(rhs) = $rhs {
                    Expression::Int(lhs $op (rhs as i32))
                } else if let Expression::IntU8(rhs) = $rhs {
                    Expression::Int(lhs $op (rhs as i32))
                } else if let Expression::Float(rhs) = $rhs {
                    Expression::Int(lhs $op rhs as i32)
                } else if let Expression::Double(rhs) = $rhs {
                    Expression::Int(lhs $op rhs as i32)
                } else if let Expression::Bool(rhs) = $rhs {
                    Expression::Int(lhs $op bool_to_int(rhs))
                } else {
                    panic!("Can't apply operator '{}' to non integer or float type: {}", $desc, $as_str);
                }
            }
            Expression::Long(lhs) => {
                if let Expression::Int(rhs) = $rhs {
                    Expression::Long(lhs $op rhs as i64)
                } else if let Expression::Long(rhs) = $rhs {
                    Expression::Long(lhs $op (rhs as i64))
                } else if let Expression::Int8(rhs) = $rhs {
                    Expression::Long(lhs $op (rhs as i64))
                } else if let Expression::IntU8(rhs) = $rhs {
                    Expression::Long(lhs $op (rhs as i64))
                } else if let Expression::Float(rhs) = $rhs {
                    Expression::Long(lhs $op rhs as i64)
                } else if let Expression::Double(rhs) = $rhs {
                    Expression::Long(lhs $op rhs as i64)
                } else if let Expression::Bool(rhs) = $rhs {
                    Expression::Long(lhs $op bool_to_int(rhs) as i64)
                } else {
                    panic!("Can't apply operator '{}' to non integer or float type: {}", $desc, $as_str);
                }
            }
            Expression::Int8(lhs) => {
                if let Expression::Int(rhs) = $rhs {
                    Expression::Int8(lhs $op rhs as i8)
                } else if let Expression::Long(rhs) = $rhs {
                    Expression::Int8(lhs $op (rhs as i8))
                } else if let Expression::Int8(rhs) = $rhs {
                    Expression::Int8(lhs $op rhs)
                } else if let Expression::IntU8(rhs) = $rhs {
                    Expression::Int8(lhs $op (rhs as i8))
                } else if let Expression::Float(rhs) = $rhs {
                    Expression::Int8(lhs $op rhs as i8)
                } else if let Expression::Double(rhs) = $rhs {
                    Expression::Int8(lhs $op rhs as i8)
                } else if let Expression::Bool(rhs) = $rhs {
                    Expression::Int8(lhs $op bool_to_int(rhs) as i8)
                } else {
                    panic!("Can't apply operator '{}' to non integer or float type: {}", $desc, $as_str);
                }
            }
            Expression::IntU8(lhs) => {
                let rhs = $rhs.to_i64();
                match $desc {
                    "+" => Expression::IntU8(lhs.wrapping_add(rhs as _)),
                    "-" => Expression::IntU8(lhs.wrapping_sub(rhs as _)),
                    "*" => Expression::Int8((lhs as i8).wrapping_mul(rhs as i8)),
                    "/" => Expression::Int8((lhs as i8).wrapping_div(rhs as i8)),
                    "%" => Expression::IntU8(lhs.wrapping_rem(rhs as _)),
                    _ => unreachable!(),
                }
            }
            Expression::Float(lhs) => {
                if let Expression::Int(rhs) = $rhs {
                    Expression::Float(lhs $op (rhs as f32))
                } else if let Expression::Long(rhs) = $rhs {
                    Expression::Float(lhs $op (rhs as f32))
                } else if let Expression::Int8(rhs) = $rhs {
                    Expression::Float(lhs $op rhs as f32)
                } else if let Expression::IntU8(rhs) = $rhs {
                    Expression::Float(lhs $op (rhs as f32))
                } else if let Expression::Float(rhs) = $rhs {
                    Expression::Float(lhs $op rhs)
                } else if let Expression::Double(rhs) = $rhs {
                    Expression::Float(lhs $op rhs as f32)
                } else if let Expression::Bool(rhs) = $rhs {
                    Expression::Float(lhs $op (bool_to_int(rhs) as f32))
                } else {
                    panic!("Can't apply operator '{}' to non integer or float type: {}", $desc, $as_str);
                }
            }
            Expression::Double(lhs) => {
                if let Expression::Int(rhs) = $rhs {
                    Expression::Double(lhs $op (rhs as f64))
                } else if let Expression::Long(rhs) = $rhs {
                    Expression::Double(lhs $op (rhs as f64))
                } else if let Expression::Int8(rhs) = $rhs {
                    Expression::Double(lhs $op rhs as f64)
                } else if let Expression::IntU8(rhs) = $rhs {
                    Expression::Double(lhs $op (rhs as f64))
                } else if let Expression::Float(rhs) = $rhs {
                    Expression::Double(lhs $op rhs as f64)
                } else if let Expression::Double(rhs) = $rhs {
                    Expression::Double(lhs $op rhs)
                } else if let Expression::Bool(rhs) = $rhs {
                    Expression::Double(lhs $op (bool_to_int(rhs) as f64))
                } else {
                    panic!("Can't apply operator '{}' to non integer or float type: {}", $desc, $as_str);
                }
            }
            Expression::Bool(lhs) => {
                if let Expression::Int(rhs) = $rhs {
                    Expression::Int(bool_to_int(lhs) $op rhs)
                } else if let Expression::IntU8(rhs) = $rhs {
                    Expression::IntU8((bool_to_int(lhs) as u8) $op rhs)
                } else if let Expression::Float(rhs) = $rhs {
                    Expression::Float((bool_to_int(lhs) as f32) $op rhs)
                } else if let Expression::Double(rhs) = $rhs {
                    Expression::Double((bool_to_int(lhs) as f64) $op rhs)
                } else if let Expression::Bool(rhs) = $rhs {
                    Expression::Int(bool_to_int(lhs) $op bool_to_int(rhs))
                } else {
                    panic!("Can't apply operator '{}' to non integer or float type: {}", $desc, $as_str);
                }
            }

            _ => panic!("Can't apply operator '{}' to non integer or float type: {}", $desc, $as_str),
        }
    }
}

macro_rules! int_value_convert {
    ($lhs:expr, $value:expr, $from:tt) => {
        match $lhs {
            Expression::Int(_) => Expression::Int($value.wrapping_add(0) as i32),
            Expression::Long(_) => Expression::Long($value.wrapping_add(0) as i64),
            Expression::Int8(_) => Expression::Int8($value.wrapping_add(0) as i8),
            Expression::IntU8(_) => Expression::IntU8($value.wrapping_add(0) as u8),
            Expression::Float(_) => Expression::Float($value.wrapping_add(0) as f32),
            Expression::Double(_) => Expression::Double($value.wrapping_add(0) as f64),
            Expression::Bool(_) => Expression::Bool($value != 0),
            _ => panic!("Can't convert {:?} to primitive type.", $from),
        }
    }
}

macro_rules! float_value_convert {
    ($lhs:expr, $value:expr, $from:tt) => {
        match $lhs {
            Expression::Int(_) => Expression::Int($value as i32),
            Expression::Long(_) => Expression::Long($value as i64),
            Expression::Int8(_) => Expression::Int8($value as i8),
            Expression::IntU8(_) => Expression::IntU8($value as u8),
            Expression::Float(_) => Expression::Float($value as f32),
            Expression::Double(_) => Expression::Double($value as f64),
            Expression::Bool(_) => Expression::Bool($value != 0.),
            _ => panic!("Can't convert {:?} to primitive type.", $from),
        }
    }
}

macro_rules! bool_value_convert {
    ($lhs:expr, $value:expr, $from:tt) => {
        match $lhs {
            Expression::Int(_) => Expression::Int(bool_to_int($value) as i32),
            Expression::Long(_) => Expression::Long(bool_to_int($value) as i64),
            Expression::Int8(_) => Expression::Int8(bool_to_int($value) as i8),
            Expression::IntU8(_) => Expression::IntU8(bool_to_int($value) as u8),
            Expression::Float(_) => Expression::Float(bool_to_int($value) as f32),
            Expression::Double(_) => Expression::Double(bool_to_int($value) as f64),
            Expression::Bool(_) => Expression::Bool($value),
            _ => panic!("Can't convert {:?} to primitive type.", $from),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Expression {
    Name(String),
    Int(i32),
    Int8(i8),
    IntU8(u8),
    Long(i64),
    Float(f32),
    Double(f64),
    Bool(bool),
    Unary {
        operator: String,
        expr: Box<Expression>,
    },
    Expr {
        lhs: Box<Expression>,
        operator: String,
        rhs: Box<Expression>,
        as_str: String,
    }
}

impl Expression {
    pub fn to_string(&self) -> String {
        match self {
            Expression::Name(v) => v.clone(),
            Expression::Int(v) => v.to_string(),
            Expression::Long(v) => v.to_string(),
            Expression::Int8(v) => v.to_string(),
            Expression::IntU8(v) => v.to_string(),
            Expression::Float(v) => v.to_string(),
            Expression::Double(v) => v.to_string(),
            Expression::Bool(v) => v.to_string(),
            Expression::Unary{ operator, expr } => {
                operator.to_owned() + &expr.to_string()
            }
            Expression::Expr{ lhs, operator, rhs, ..} => {
                lhs.to_string() + operator + &rhs.to_string()
            }
        }
    }

    pub fn convert_to(&self, ref_type: &Expression) -> Expression {
        match self {
            Expression::Name(v) => Expression::Name(v.clone()),
            Expression::Int(v) => int_value_convert!(ref_type, *v, self),
            Expression::Long(v) => int_value_convert!(ref_type, *v, self),
            Expression::Int8(v) => int_value_convert!(ref_type, *v, self),
            Expression::IntU8(v) => int_value_convert!(ref_type, *v, self),
            Expression::Float(v) => float_value_convert!(ref_type, *v, self),
            Expression::Double(v) => float_value_convert!(ref_type, *v, self),
            Expression::Bool(v) => bool_value_convert!(ref_type, *v, self),
            Expression::Unary{ operator, expr } => {
                let expr = expr.calc_unary(operator, &mut HashMap::new());
                expr.convert_to(ref_type)
            }
            Expression::Expr{ lhs, operator, rhs, .. } => {
                let expr = lhs.calc_expr(operator, rhs, &mut HashMap::new());
                expr.convert_to(ref_type)
            }
        }
    }

    pub fn to_expr(&self) -> (Box<Expression>, String, Box<Expression>, String) {
        if let Expression::Expr{lhs, operator, rhs, as_str} = self {
            (lhs.clone(), operator.clone(), rhs.clone(), as_str.clone())
        } else {
            panic!("Can't convert {:?} to expr", self)
        }
    }

    pub fn to_unary(&self) -> (String, Box<Expression>) {
        if let Expression::Unary{operator, expr} = self {
            (operator.clone(), expr.clone())
        } else {
            panic!("Can't convert {:?} to unary", self)
        }
    }

    pub fn to_bool(&self) -> bool {
        match self {
            Expression::Name(v) => v.is_empty() != true,
            Expression::Int(v) => *v != 0,
            Expression::Long(v) => *v != 0,
            Expression::Int8(v) => *v != 0,
            Expression::IntU8(v) => *v != 0,
            Expression::Float(v) => *v != 0.,
            Expression::Double(v) => *v != 0.,
            Expression::Bool(v) => *v,
            Expression::Unary{ operator, expr } => {
                let expr = expr.calc_unary(operator, &mut HashMap::new());
                expr.to_bool()
            }
            Expression::Expr{ lhs, operator, rhs, .. } => {
                let expr = lhs.calc_expr(operator, rhs, &mut HashMap::new());
                expr.to_bool()
            }
        }
    }

    pub fn to_i64(&self) -> i64 {
        match self {
            Expression::Name(v) => panic!("Can't convert Name {} to i64", v),
            Expression::Int(v) => *v as i64,
            Expression::Long(v) => *v as i64,
            Expression::Int8(v) => *v as i64,
            Expression::IntU8(v) => *v as i8 as i64,
            Expression::Float(v) => *v as i64,
            Expression::Double(v) => *v as i64,
            Expression::Bool(v) => bool_to_int(*v) as i64,
            Expression::Unary{ operator, expr } => {
                let expr = expr.calc_unary(operator, &mut HashMap::new());
                expr.to_i64()
            }
            Expression::Expr{ lhs, operator, rhs, .. } => {
                let expr = lhs.calc_expr(operator, rhs, &mut HashMap::new());
                expr.to_i64()
            }
        }
    }

    pub fn to_f64(&self) -> f64 {
        match self {
            Expression::Name(v) => panic!("Can't convert Name {} to f64", v),
            Expression::Int(v) => *v as f64,
            Expression::Long(v) => *v as f64,
            Expression::Int8(v) => *v as f64,
            Expression::IntU8(v) => *v as f64,
            Expression::Float(v) => *v as f64,
            Expression::Double(v) => *v as f64,
            Expression::Bool(v) => bool_to_int(*v) as f64,
            Expression::Unary{ operator, expr } => {
                let expr = expr.calc_unary(operator, &mut HashMap::new());
                expr.to_f64()
            }
            Expression::Expr{ lhs, operator, rhs, .. } => {
                let expr = lhs.calc_expr(operator, rhs, &mut HashMap::new());
                expr.to_f64()
            }
        }
    }

    pub fn as_str(&self) -> String {
        match self {
            Expression::Name(v) => v.clone(),
            Expression::Int(v) => v.to_string(),
            Expression::Long(v) => v.to_string(),
            Expression::Int8(v) => v.to_string(),
            Expression::IntU8(v) => v.to_string(),
            Expression::Float(v) => v.to_string(),
            Expression::Double(v) => v.to_string(),
            Expression::Bool(v) => v.to_string(),
            Expression::Unary{ operator, expr } => {
                format!("{operator}{}", expr.as_str())
            }
            Expression::Expr{ lhs: _, operator: _, rhs: _, as_str } => {
                as_str.into()
            }
        }
    }

    fn calc_expr(&self, operator: &str, rhs: &Expression, dict: &mut HashMap<String, ConstExpr>) -> Expression {
        let lhs = self.calculate(dict);
        let rhs = rhs.calculate(dict);

        match operator {
            "||" => {
                return Expression::Bool(lhs.to_bool() || rhs.to_bool())
            }
            "&&" => {
                return Expression::Bool(lhs.to_bool() && rhs.to_bool())
            }
            "|" => {
                // Makes same type.
                let rhs = rhs.convert_to(&lhs);

                match lhs {
                    Expression::Int(_v) => Expression::Int((lhs.to_i64() | rhs.to_i64()) as i32),
                    Expression::Long(_v) => Expression::Long(lhs.to_i64() | rhs.to_i64()),
                    Expression::Int8(_v) => Expression::Int8((lhs.to_i64() | rhs.to_i64()) as i8),
                    Expression::IntU8(_v) => Expression::IntU8((lhs.to_i64() | rhs.to_i64()) as u8),
                    Expression::Bool(_v) => Expression::Bool((lhs.to_i64() | rhs.to_i64()) != 0),
                    _ => panic!("Can't apply operator '|' for non integer type: {} {}", lhs.as_str(), rhs.as_str()),
                }
            }
            "^" => {
                // Makes same type.
                let rhs = rhs.convert_to(&lhs);

                match lhs {
                    Expression::Int(_v) => Expression::Int((lhs.to_i64() ^ rhs.to_i64()) as i32),
                    Expression::Long(_v) => Expression::Long(lhs.to_i64() ^ rhs.to_i64()),
                    Expression::Int8(_v) => Expression::Int8((lhs.to_i64() ^ rhs.to_i64()) as i8),
                    Expression::IntU8(_v) => Expression::IntU8((lhs.to_i64() ^ rhs.to_i64()) as u8),
                    Expression::Bool(_v) => Expression::Bool((lhs.to_i64() ^ rhs.to_i64()) != 0),
                    _ => panic!("Can't apply operator '^' for non integer type: {} {}", lhs.as_str(), rhs.as_str()),
                }
            }
            "&" => {
                // Makes same type.
                let rhs = rhs.convert_to(&lhs);

                match lhs {
                    Expression::Int(_v) => Expression::Int((lhs.to_i64() & rhs.to_i64()) as i32),
                    Expression::Long(_v) => Expression::Long(lhs.to_i64() & rhs.to_i64()),
                    Expression::Int8(_v) => Expression::Int8((lhs.to_i64() & rhs.to_i64()) as i8),
                    Expression::IntU8(_v) => Expression::IntU8((lhs.to_i64() & rhs.to_i64()) as u8),
                    Expression::Bool(_v) => Expression::Bool((lhs.to_i64() & rhs.to_i64()) != 0),
                    _ => panic!("Can't apply operator '&' for non integer type: {} {}", lhs.as_str(), rhs.as_str()),
                }
            }
            "==" => {
                // println!("{:?} == {:?} => {:?}", lhs, rhs, lhs.partial_cmp(&rhs));
                Expression::Bool(lhs == rhs)
            }
            "!=" => {
                Expression::Bool(lhs != rhs)
            }
            "<<" => {
                let rhs = rhs.to_i64() as u32;

                match lhs {
                    Expression::Int(v) => Expression::Int(v.wrapping_shl(rhs)),
                    Expression::Long(v) => Expression::Long(v.wrapping_shl(rhs)),
                    Expression::Int8(v) => Expression::Int8(v.wrapping_shl(rhs)),
                    Expression::IntU8(v) => Expression::IntU8(v.wrapping_shl(rhs)),
                    Expression::Bool(v) => Expression::Bool(((v as u64).wrapping_shl(rhs)) != 0),
                    _ => panic!("Can't apply operator '<<' for non integer type: {}", lhs.as_str()),
                }
            }
            ">>" => {
                let rhs = rhs.to_i64() as u32;

                match lhs {
                    Expression::Int(v) => Expression::Int(v.wrapping_shr(rhs)),
                    Expression::Long(v) => Expression::Long(v.wrapping_shr(rhs)),
                    Expression::Int8(v) => Expression::Int8(v.wrapping_shr(rhs)),
                    Expression::IntU8(v) => Expression::IntU8(v.wrapping_shr(rhs)),
                    Expression::Bool(v) => Expression::Bool((v as u64).wrapping_shr(rhs) != 0),
                    _ => panic!("Can't apply operator '>>' for non integer type: {}", lhs.as_str()),
                }
            }
            "<=" => Expression::Bool(lhs <= rhs),
            ">=" => Expression::Bool(lhs >= rhs),
            "<" => Expression::Bool(lhs < rhs),
            ">" => Expression::Bool(lhs > rhs),
            "+" => arithmetic_op!(lhs, +, rhs, "+", self.as_str()),
            "-" => arithmetic_op!(lhs, -, rhs, "-", self.as_str()),
            "*" => arithmetic_op!(lhs, *, rhs, "*", self.as_str()),
            "/" => arithmetic_op!(lhs, /, rhs, "/", self.as_str()),
            "%" => arithmetic_op!(lhs, %, rhs, "%", self.as_str()),
            _ => unreachable!(),
        }
    }

    fn calc_unary(&self, operator: &str, dict: &mut HashMap<String, ConstExpr>) -> Expression {
        match self {
            Expression::Name(_) => {
                let expr = self.calculate(dict);
                return expr.calc_unary(operator, dict)
            }
            Expression::Unary{operator: op, expr} => {
                let expr = expr.calc_unary(op, dict);
                return expr.calc_unary(operator, dict)
            }
            Expression::Expr{lhs, operator: op, rhs, .. } => {
                let expr = lhs.calc_expr(op, rhs, dict);
                return expr.calc_unary(operator, dict)
            }
            _ => {}
        }

        match operator {
            "+" => self.clone(),
            "-" => {
                match self {
                    Expression::Int(v) => Self::Int(-v),
                    Expression::Long(v) => Self::Long(-(*v)),
                    Expression::Int8(v) => Self::Int8(-v),
                    Expression::IntU8(v) => Self::Int8(-(*v as i8)),
                    Expression::Float(v) => Self::Float(-v),
                    Expression::Double(v) => Self::Double(-v),
                    Expression::Bool(v) => Self::Int(-bool_to_int(*v)),
                    _ => panic!("Can't apply unary operator '-' to {:?}\n{}", self, self.as_str()),
                }
            }
            "~" | "!" => {
                match self {
                    Expression::Int(v) => Self::Int(!v),
                    Expression::Long(v) => Self::Long(!v),
                    Expression::Int8(v) => Self::Int8(!v),
                    Expression::IntU8(v) => Self::IntU8(!v),
                    Expression::Bool(v) => Self::Bool(!v),
                    _ => panic!("Can't apply unary operator '~' or \"!\" to {:?}\n{}", self, self.as_str()),
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn calculate(&self, dict: &mut HashMap<String, ConstExpr>) -> Expression {
        match self {
            Expression::Name(name) => {
                let v = dict[name].clone();
                let res = v.calculate(dict);
                if let ConstExpr::Expression(expr) = res.clone() {
                    dict.insert(name.to_owned(), res);
                    expr
                } else {
                    panic!("Error in Expression calculate(): {}\n{:?}", self.as_str(), self);
                }
            }
            Expression::Unary{operator, expr} => {
                let expr = expr.calculate(dict);
                expr.calc_unary(operator, dict)
            }
            Expression::Expr{lhs, operator, rhs, .. } => {
                lhs.calc_expr(operator, rhs, dict)
            }
            _ => self.clone(),
        }
    }

}

impl PartialEq for Expression {
    fn eq(&self, rhs: &Self) -> bool {
        self.partial_cmp(rhs) == Some(std::cmp::Ordering::Equal)
    }
}

impl PartialOrd for Expression {
    fn partial_cmp(&self, rhs: &Self) -> Option<std::cmp::Ordering> {
        match self {
            Expression::Name(l_name) => {
                if let Expression::Name(r_name) = rhs {
                    l_name.partial_cmp(r_name)
                } else {
                    None
                }
            }
            Expression::Int(_) |
            Expression::Int8(_) |
            Expression::Long(_) |
            Expression::Bool(_) => {
                let lhs = self.to_i64();
                let rhs = rhs.to_i64();

                if lhs < rhs {
                    Some(std::cmp::Ordering::Less)
                } else if lhs == rhs {
                    Some(std::cmp::Ordering::Equal)
                } else {
                    Some(std::cmp::Ordering::Greater)
                }
            }
            Expression::IntU8(lhs) => {
                let rhs = rhs.to_i64() as i8 as u8;

                if *lhs < rhs {
                    Some(std::cmp::Ordering::Less)
                } else if *lhs == rhs {
                    Some(std::cmp::Ordering::Equal)
                } else {
                    Some(std::cmp::Ordering::Greater)
                }
            }

            Expression::Double(_) |
            Expression::Float(_) => {
                let lhs = self.to_f64();
                let rhs = rhs.to_f64();

                if lhs < rhs {
                    Some(std::cmp::Ordering::Less)
                } else if lhs == rhs {
                    Some(std::cmp::Ordering::Equal)
                } else {
                    Some(std::cmp::Ordering::Greater)
                }
            }

            _ => format!("{:?}", self).partial_cmp(&format!("{:?}", rhs)),
        }
    }
}


#[derive(Debug, Clone, PartialEq)]
pub enum StringExpr {
    CStr(String),
    Name(String),
    List(Vec<Box<StringExpr>>),
}

impl StringExpr {
    pub fn to_string(&self) -> String {
        match self {
            StringExpr::CStr(v) => v.clone(),
            StringExpr::Name(v) => v.clone(),
            StringExpr::List(list) => {
                let mut res = "".to_string();

                for v in list {
                    res += &v.to_string();
                }

                res
            }
        }
    }

    pub fn calculate(&self, dict: &mut HashMap<String, ConstExpr>) -> StringExpr {
        match self {
            StringExpr::Name(name) => {
                let v = dict[name].clone();
                let res = v.calculate(dict);
                if let ConstExpr::String(cstr) = res.clone() {
                    dict.insert(name.to_owned(), res);
                    cstr
                } else {
                    panic!("Wrong format is used {:?}", self);
                }
            }
            StringExpr::List(list) => {
                let mut cstr = String::new();

                for v in list {
                    if let StringExpr::CStr(v) = v.calculate(dict) {
                        cstr += &v;
                    } else {
                        panic!("Wrong format is used {:?}", self);
                    }
                }

                StringExpr::CStr(cstr)
            }
            // StringExpr::Expr{lhs, rhs} => {
            //     let lhs = if let StringExpr::CStr(lhs) = lhs.calculate(dict)? {
            //         lhs
            //     } else {
            //         return Err(Error::WrongFormat(format!("Wrong format is used {:?}", self)))
            //     };
            //     let rhs = if let StringExpr::CStr(rhs) = rhs.calculate(dict)? {
            //         rhs
            //     } else {
            //         return Err(Error::WrongFormat(format!("Wrong format is used {:?}", self)))
            //     };

            //     Ok(StringExpr::CStr(lhs + &rhs))
            // }
            _ => self.clone(),
        }

    }
}

#[derive(Debug, Clone)]
pub enum ConstExpr {
    Char(char),
    String(StringExpr),
    Expression(Expression),
    List(Vec<Box<ConstExpr>>),
}

impl Default for ConstExpr {
    fn default() -> Self {
        Self::Expression(Expression::Int(0))
    }
}

impl ConstExpr {
    pub fn to_string(&self) -> String {
        match self {
            ConstExpr::Char(v) => v.to_string(),
            ConstExpr::String(v) => v.to_string(),
            ConstExpr::List(list) => {
                let mut res = "vec![".to_owned();
                for v in list {
                    res += &(v.to_string() + ",");
                }

                res += "]";

                res
            }
            ConstExpr::Expression(v) => v.to_string(),
        }
    }

    fn to_char(&self) -> ConstExpr {
        match self {
            ConstExpr::Char(_) => self.clone(),
            ConstExpr::String(_) => panic!("Can't convert from String to Char."),
            ConstExpr::List(list) => {
                let mut res = Vec::new();
                for const_expr in list {
                    res.push(Box::new(const_expr.to_char()))
                }
                ConstExpr::List(res)
            }
            ConstExpr::Expression(_) => panic!("Can't convert from Expression to Char."),
        }
    }

    fn to_string_expr(&self) -> ConstExpr {
        match self {
            ConstExpr::Char(_) => panic!("Can't convert from Char to StringExpr."),
            ConstExpr::String(_) => self.clone(),
            ConstExpr::List(list) => {
                let mut res = Vec::new();
                for const_expr in list {
                    res.push(Box::new(const_expr.to_string_expr()))
                }
                ConstExpr::List(res)
            }
            ConstExpr::Expression(_) => panic!("Can't convert from Expression to StringExpr."),
        }
    }

    pub fn convert_to(&self, expr: &ConstExpr) -> ConstExpr {
        match expr {
            ConstExpr::Char(_) => self.to_char(),
            ConstExpr::String(_) => self.to_string_expr(),
            ConstExpr::List(expr) => self.to_list(&expr[0]), //*list = other.to_list(mut expr),
            ConstExpr::Expression(expr) => self.to_expression(expr),
        }
    }

    fn to_expression(&self, arg_expr: &Expression) -> ConstExpr {
        match self {
            ConstExpr::Expression(v) => ConstExpr::Expression(v.convert_to(arg_expr)),
            ConstExpr::List(list) => {
                let mut res = Vec::new();
                for expr in list {
                    res.push(Box::new(expr.convert_to(&ConstExpr::Expression(arg_expr.clone()))))
                }
                ConstExpr::List(res)
            }
            _ => panic!("Can't convert {:?} to Expression {:?}", self, arg_expr),
        }
    }

    fn to_list(&self, arg_expr: &ConstExpr) -> ConstExpr {
        match self {
            ConstExpr::Expression(_) |
            ConstExpr::String(_) |
            ConstExpr::Char(_) => ConstExpr::List(vec![Box::new(self.convert_to(arg_expr))]),
            ConstExpr::List(list) => {
                let mut res = Vec::new();
                for expr in list {
                    res.push(Box::new(expr.convert_to(arg_expr)))
                }

                ConstExpr::List(res)
            }
        }
    }

    pub fn calculate(&self, dict: &mut HashMap<String, ConstExpr>) -> ConstExpr {
        let res = match self {
            ConstExpr::Char(_v) => self.clone(),
            ConstExpr::String(v) => {
                ConstExpr::String(v.calculate(dict))
            }
            ConstExpr::List(list) => {
                let mut res = Vec::new();
                for v in list {
                    res.push(Box::new(v.calculate(dict)))
                }
                ConstExpr::List(res)
            }
            ConstExpr::Expression(v) => {
                ConstExpr::Expression(v.calculate(dict))
            }
        };
        res
    }
}

// impl PartialOrd for ConstExpr {
//     fn partial_cmp(&self, rhs: &Self) -> Option<std::cmp::Ordering> {
//         match self {
//             ConstExpr::Int(lhs) => {
//                 let rhs = if let ConstExpr::Int(rhs) = rhs {
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
        let mut dict = HashMap::new();
        let expr = Expression::Expr {
            lhs: Box::new(Expression::Int(10)),
            operator: "+".to_owned(),
            rhs: Box::new(Expression::Int(10)),
            as_str: "".into(),
        };

        assert_eq!(expr.calculate(&mut dict), Expression::Int(20));

        let expr = Expression::Expr {
            lhs: Box::new(Expression::IntU8(10)),
            operator: "-".to_owned(),
            rhs: Box::new(Expression::IntU8(10)),
            as_str: "".into(),
        };

        assert_eq!(expr.calculate(&mut dict), Expression::IntU8(0));

        let expr = Expression::Expr {
            lhs: Box::new(Expression::Int(10)),
            operator: "*".to_owned(),
            rhs: Box::new(Expression::IntU8(10)),
            as_str: "".into(),
        };

        assert_eq!(expr.calculate(&mut dict), Expression::Int(100));

        let expr = Expression::Expr {
            lhs: Box::new(Expression::Int(10)),
            operator: "/".to_owned(),
            rhs: Box::new(Expression::Float(2.0)),
            as_str: "".into(),
        };

        assert_eq!(expr.calculate(&mut dict), Expression::Float(5.0));

        let expr = Expression::Expr {
            lhs: Box::new(Expression::Float(10.0)),
            operator: "%".to_owned(),
            rhs: Box::new(Expression::Float(2.0)),
            as_str: "".into(),
        };

        assert_eq!(expr.calculate(&mut dict), Expression::Float(10.0 % 2.0));

        let expr = Expression::Expr {
            lhs: Box::new(Expression::Int(10)),
            operator: "%".to_owned(),
            rhs: Box::new(Expression::Bool(true)),
            as_str: "".into(),
        };

        assert_eq!(expr.calculate(&mut dict), Expression::Int(10 % 1));
    }

    #[test]
    fn test_const_expr_name() {
        let mut dict = HashMap::new();
        dict.insert("DUMP_FLAG_PRIORITY_CRITICAL".to_owned(),
            ConstExpr::Expression(Expression::Expr {
                lhs: Box::new(Expression::Int(1)),
                operator: "<<".to_owned(),
                rhs: Box::new(Expression::Int(0)),
                as_str: "".into(),
            })
        );

        dict.insert("DUMP_FLAG_PRIORITY_HIGH".to_owned(),
            ConstExpr::Expression(Expression::Expr {
                lhs: Box::new(Expression::Int(1)),
                operator: "<<".to_owned(),
                rhs: Box::new(Expression::Int(1)),
                as_str: "".into(),
        })
        );

        dict.insert("DUMP_FLAG_PRIORITY_NORMAL".to_owned(),
            ConstExpr::Expression(Expression::Expr {
                lhs: Box::new(Expression::Int(1)),
                operator: "<<".to_owned(),
                rhs: Box::new(Expression::Int(2)),
                as_str: "".into(),
            })
        );

        dict.insert("DUMP_FLAG_PRIORITY_DEFAULT".to_owned(),
            ConstExpr::Expression(Expression::Expr {
                lhs: Box::new(Expression::Int(1)),
                operator: "<<".to_owned(),
                rhs: Box::new(Expression::Int(3)),
                as_str: "".into(),
        })
        );


        let expr = Expression::Expr {
            as_str: "".into(),
            lhs: Box::new(Expression::Name("DUMP_FLAG_PRIORITY_CRITICAL".to_owned())),
            operator: "|".to_owned(),
            rhs: Box::new(Expression::Expr{
                as_str: "".into(),
                lhs: Box::new(Expression::Name("DUMP_FLAG_PRIORITY_HIGH".to_owned())),
                operator: "|".to_owned(),
                rhs: Box::new(Expression::Expr{
                    lhs: Box::new(Expression::Name("DUMP_FLAG_PRIORITY_NORMAL".to_owned())),
                    operator: "|".to_owned(),
                    rhs: Box::new(Expression::Name("DUMP_FLAG_PRIORITY_DEFAULT".to_owned())),
                    as_str: "".into(),
                }),
            }),
        };

        assert_eq!(expr.calculate(&mut dict), Expression::Int(15));
    }
}