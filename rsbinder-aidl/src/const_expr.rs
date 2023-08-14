use std::collections::HashMap;
use std::fmt;

#[derive(Debug)]
pub enum Error {
    NotApplyType(String),
    WrongFormat(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::NotApplyType(s) => write!(f, "NotApplyType: {}", s),
            Error::WrongFormat(s) => write!(f, "WrongFormat: {}", s),
        }
    }
}

impl std::error::Error for Error {}

fn bool_to_int(v: bool) -> i64 {
    if v == true { 1 } else { 0 }
}

macro_rules! arithmetic_op {
    ($lhs:expr, $op:tt, $rhs:expr, $desc:expr) => {
        match $lhs {
            Expression::Int(lhs) => {
                if let Expression::Int(rhs) = $rhs {
                    Ok(Expression::Int(lhs $op rhs))
                } else if let Expression::IntU8(rhs) = $rhs {
                    Ok(Expression::Int(lhs $op (rhs as i64)))
                } else if let Expression::Float(rhs) = $rhs {
                    Ok(Expression::Float((lhs as f64) $op rhs))
                } else if let Expression::Bool(rhs) = $rhs {
                    Ok(Expression::Int(lhs $op bool_to_int(rhs)))
                } else {
                    Err(Error::NotApplyType(format!("Can't apply '{}' operator for non integer or float type.", $desc)))
                }
            }
            Expression::IntU8(lhs) => {
                if let Expression::Int(rhs) = $rhs {
                    Ok(Expression::Int((lhs as i64) $op rhs))
                } else if let Expression::IntU8(rhs) = $rhs {
                    Ok(Expression::IntU8(lhs $op rhs))
                } else if let Expression::Float(rhs) = $rhs {
                    Ok(Expression::Float((lhs as f64) $op rhs))
                } else if let Expression::Bool(rhs) = $rhs {
                    Ok(Expression::IntU8(lhs $op (bool_to_int(rhs) as u8)))
                } else {
                    Err(Error::NotApplyType(format!("Can't apply '{}' operator for non integer or float type.", $desc)))
                }
            }
            Expression::Float(lhs) => {
                if let Expression::Int(rhs) = $rhs {
                    Ok(Expression::Float(lhs $op (rhs as f64)))
                } else if let Expression::IntU8(rhs) = $rhs {
                    Ok(Expression::Float(lhs $op (rhs as f64)))
                } else if let Expression::Float(rhs) = $rhs {
                    Ok(Expression::Float(lhs $op rhs))
                } else if let Expression::Bool(rhs) = $rhs {
                    Ok(Expression::Float(lhs $op (bool_to_int(rhs) as f64)))
                } else {
                    Err(Error::NotApplyType(format!("Can't apply '{}' operator for non integer or float type.", $desc)))
                }
            }
            Expression::Bool(lhs) => {
                if let Expression::Int(rhs) = $rhs {
                    Ok(Expression::Int(bool_to_int(lhs) $op rhs))
                } else if let Expression::IntU8(rhs) = $rhs {
                    Ok(Expression::IntU8((bool_to_int(lhs) as u8) $op rhs))
                } else if let Expression::Float(rhs) = $rhs {
                    Ok(Expression::Float((bool_to_int(lhs) as f64) $op rhs))
                } else if let Expression::Bool(rhs) = $rhs {
                    Ok(Expression::Int(bool_to_int(lhs) $op bool_to_int(rhs)))
                } else {
                    Err(Error::NotApplyType(format!("Can't apply '{}' operator for non integer or float type.", $desc)))
                }
            }

            _ => Err(Error::NotApplyType(format!("Can't apply '{}' operator for non integer or float type.", $desc))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    Name(String),
    Int(i64),
    IntU8(u8),
    Float(f64),
    Bool(bool),
    Unary {
        operator: String,
        expr: Box<Expression>,
    },
    Expr {
        lhs: Box<Expression>,
        operator: String,
        rhs: Box<Expression>,
    }
}

impl Expression {
    pub fn to_string(&self) -> String {
        match self {
            Expression::Name(v) => v.clone(),
            Expression::Int(v) => v.to_string(),
            Expression::IntU8(v) => v.to_string(),
            Expression::Float(v) => v.to_string(),
            Expression::Bool(v) => v.to_string(),
            Expression::Unary{ operator, expr } => {
                operator.to_owned() + &expr.to_string()
            }
            Expression::Expr{ lhs, operator, rhs} => {
                lhs.to_string() + operator + &rhs.to_string()
            }
        }
    }

    pub fn to_i64(&self) -> Result<i64, Error> {
        match self {
            Expression::Name(v) => Err(Error::WrongFormat(format!("Can't convert Name {} to i64", v))),
            Expression::Int(v) => Ok(*v),
            Expression::IntU8(v) => Ok(*v as i64),
            Expression::Float(v) => Ok(*v as i64),
            Expression::Bool(v) => if *v == true { Ok(1) } else { Ok(0) },
            Expression::Unary{ operator, expr } => {
                let expr = expr.calc_unary(operator, &mut HashMap::new())?;
                expr.to_i64()
            }
            Expression::Expr{ lhs, operator, rhs} => {
                let expr = lhs.calc_expr(operator, rhs, &mut HashMap::new())?;
                expr.to_i64()
            }
        }
    }

    fn calc_expr(&self, operator: &str, rhs: &Expression, dict: &mut HashMap<String, ConstExpr>) -> Result<Expression, Error> {
        let lhs = self.calculate(dict)?;
        let rhs = rhs.calculate(dict)?;

        match operator {
            "||" => {
                if let Expression::Bool(lhs) = lhs {
                    if let Expression::Bool(rhs) = rhs {
                        return Ok(Expression::Bool(lhs || rhs))
                    }
                }
                Err(Error::NotApplyType("Can't apply operator '||' to non bool type".to_owned()))
            }
            "&&" => {
                if let Expression::Bool(lhs) = lhs {
                    if let Expression::Bool(rhs) = rhs {
                        return Ok(Expression::Bool(lhs && rhs))
                    }
                }
                Err(Error::NotApplyType("Can't apply operator '&&' to non bool type".to_owned()))
            }
            "|" => {
                match lhs {
                    Expression::Int(lhs) => {
                        if let Expression::Int(rhs) = rhs {
                            return Ok(Expression::Int(lhs | rhs))
                        } else if let Expression::IntU8(rhs) = rhs {
                            return Ok(Expression::Int(lhs | (rhs as i64)))
                        }
                    }
                    Expression::IntU8(lhs) => {
                        if let Expression::IntU8(rhs) = rhs {
                            return Ok(Expression::IntU8(lhs | rhs))
                        } else if let Expression::Int(rhs) = rhs {
                            return Ok(Expression::Int((lhs as i64) | rhs))
                        }
                    }
                    _ => {}
                }

                Err(Error::NotApplyType("Can't apply operator '|' to non integer type".to_owned()))
            }
            "^" => {
                match lhs {
                    Expression::Int(lhs) => {
                        if let Expression::Int(rhs) = rhs {
                            return Ok(Expression::Int(lhs ^ rhs))
                        } else if let Expression::IntU8(rhs) = rhs {
                            return Ok(Expression::Int(lhs ^ (rhs as i64)))
                        }
                    }
                    Expression::IntU8(lhs) => {
                        if let Expression::IntU8(rhs) = rhs {
                            return Ok(Expression::IntU8(lhs ^ rhs))
                        } else if let Expression::Int(rhs) = rhs {
                            return Ok(Expression::Int((lhs as i64) ^ rhs))
                        }
                    }
                    _ => {}
                }

                Err(Error::NotApplyType("Can't apply operator '^' to non integer type".to_owned()))
            }
            "&" => {
                match lhs {
                    Expression::Int(lhs) => {
                        if let Expression::Int(rhs) = rhs {
                            return Ok(Expression::Int(lhs & rhs))
                        } else if let Expression::IntU8(rhs) = rhs {
                            return Ok(Expression::Int(lhs & (rhs as i64)))
                        }
                    }
                    Expression::IntU8(lhs) => {
                        if let Expression::IntU8(rhs) = rhs {
                            return Ok(Expression::IntU8(lhs & rhs))
                        } else if let Expression::Int(rhs) = rhs {
                            return Ok(Expression::Int((lhs as i64) & rhs))
                        }
                    }
                    _ => {}
                }

                Err(Error::NotApplyType("Can't apply operator '&' to non integer type".to_owned()))
            }
            "==" => {
                Ok(Expression::Bool(lhs == rhs))
            }
            "!=" => {
                Ok(Expression::Bool(lhs != rhs))
            }
            "<<" | ">>" => {
                let rhs = if let Expression::Int(rhs) = rhs {
                    rhs
                } else if let Expression::IntU8(rhs) = rhs {
                    rhs as _
                } else {
                    return Err(Error::NotApplyType("Can't apply operator '<<' to non integer type".to_owned()));
                };

                if let Expression::Int(lhs) = lhs {
                    if operator == "<<" {
                        return Ok(Expression::Int(lhs << rhs));
                    } else {
                        return Ok(Expression::Int(lhs >> rhs));
                    }
                } else if let Expression::IntU8(lhs) = lhs {
                    if operator == "<<" {
                        return Ok(Expression::IntU8(lhs << rhs));
                    } else {
                        return Ok(Expression::IntU8(lhs >> rhs));
                    }
                }
                Err(Error::NotApplyType("Can't apply operator '<<' to non integer type".to_owned()))
            }
            "<=" => Ok(Expression::Bool(lhs <= rhs)),
            ">=" => Ok(Expression::Bool(lhs >= rhs)),
            "<" => Ok(Expression::Bool(lhs < rhs)),
            ">" => Ok(Expression::Bool(lhs > rhs)),
            "+" => arithmetic_op!(lhs, +, rhs, "+"),
            "-" => arithmetic_op!(lhs, -, rhs, "-"),
            "*" => arithmetic_op!(lhs, *, rhs, "*"),
            "/" => arithmetic_op!(lhs, /, rhs, "/"),
            "%" => arithmetic_op!(lhs, %, rhs, "%"),
            _ => unreachable!(),
        }
    }

    fn calc_unary(&self, operator: &str, dict: &mut HashMap<String, ConstExpr>) -> Result<Expression, Error> {
        match self {
            Expression::Name(_) => {
                let expr = self.calculate(dict)?;
                return expr.calc_unary(operator, dict)
            }
            Expression::Unary{operator: op, expr} => {
                let expr = expr.calc_unary(op, dict)?;
                return expr.calc_unary(operator, dict)
            }
            Expression::Expr{lhs, operator: op, rhs} => {
                let expr = lhs.calc_expr(op, rhs, dict)?;
                return expr.calc_unary(operator, dict)
            }
            _ => {}
        }

        match operator {
            "+" => Ok(self.clone()),
            "-" => {
                match self {
                    Expression::Int(v) => Ok(Self::Int(-v)),
                    Expression::IntU8(v) => Ok(Self::Int(-(*v as i64))),
                    Expression::Float(v) => Ok(Self::Float(-v)),
                    Expression::Bool(v) => Ok(Self::Int(-bool_to_int(*v))),
                    _ => Err(Error::NotApplyType(format!("Can't apply unary operator '-' to {:?}", self))),
                }
            }
            "~" | "!" => {
                match self {
                    Expression::Int(v) => Ok(Self::Int(!v)),
                    Expression::IntU8(v) => Ok(Self::IntU8(!v)),
                    Expression::Bool(v) => Ok(Self::Bool(!v)),
                    _ => Err(Error::NotApplyType(format!("Can't apply unary operator '~' or \"!\" to {:?}", self))),
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn calculate(&self, dict: &mut HashMap<String, ConstExpr>) -> Result<Expression, Error> {
        match self {
            Expression::Name(name) => {
                let v = dict[name].clone();
                let res = v.calculate(dict)?;
                if let ConstExpr::Expression(expr) = res.clone() {
                    dict.insert(name.to_owned(), res);
                    Ok(expr)
                } else {
                    Err(Error::WrongFormat(format!("Wrong format is used {:?}", self)))
                }
            }
            Expression::Unary{operator, expr} => {
                let expr = expr.calculate(dict)?;
                expr.calc_unary(operator, dict)
            }
            Expression::Expr{lhs, operator, rhs} => {
                lhs.calc_expr(operator, rhs, dict)
            }
            _ => Ok(self.clone()),
        }
    }

}

impl PartialOrd for Expression {
    fn partial_cmp(&self, rhs: &Self) -> Option<std::cmp::Ordering> {
        match self {
            Expression::Int(lhs) => {
                let rhs = if let Expression::Int(rhs) = rhs {
                    *rhs
                } else if let Expression::IntU8(rhs) = rhs {
                    *rhs as i64
                } else {
                    return None
                };

                if lhs < &rhs {
                    Some(std::cmp::Ordering::Less)
                } else if lhs == &rhs {
                    Some(std::cmp::Ordering::Equal)
                } else {
                    Some(std::cmp::Ordering::Greater)
                }
            }
            Expression::Float(lhs) => {
                let rhs = if let Expression::Float(rhs) = rhs {
                    *rhs
                } else {
                    return None
                };
                if lhs < &rhs {
                    Some(std::cmp::Ordering::Less)
                } else if lhs == &rhs {
                    Some(std::cmp::Ordering::Equal)
                } else {
                    Some(std::cmp::Ordering::Greater)
                }
            }
            _ => return None
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

    pub fn calculate(&self, dict: &mut HashMap<String, ConstExpr>) -> Result<StringExpr, Error> {
        match self {
            StringExpr::Name(name) => {
                let v = dict[name].clone();
                let res = v.calculate(dict)?;
                if let ConstExpr::String(cstr) = res.clone() {
                    dict.insert(name.to_owned(), res);
                    Ok(cstr)
                } else {
                    Err(Error::WrongFormat(format!("Wrong format is used {:?}", self)))
                }
            }
            StringExpr::List(list) => {
                let mut cstr = String::new();

                for v in list {
                    if let StringExpr::CStr(v) = v.calculate(dict)? {
                        cstr += &v;
                    } else {
                        return Err(Error::WrongFormat(format!("Wrong format is used {:?}", self)))
                    }
                }

                Ok(StringExpr::CStr(cstr))
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
            _ => Ok(self.clone()),
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
                let mut res = String::new();
                for v in list {
                    res += &v.to_string();
                }

                res
            }
            ConstExpr::Expression(v) => v.to_string(),
        }
    }

    pub fn calculate(&self, dict: &mut HashMap<String, ConstExpr>) -> Result<ConstExpr, Error> {
        let res = match self {
            ConstExpr::Char(_v) => self.clone(),
            ConstExpr::String(v) => {
                ConstExpr::String(v.calculate(dict)?)
            }
            ConstExpr::List(list) => {
                let mut res = Vec::new();
                for v in list {
                    res.push(Box::new(v.calculate(dict)?))
                }
                ConstExpr::List(res)
            }
            ConstExpr::Expression(v) => {
                ConstExpr::Expression(v.calculate(dict)?)
            }
        };
        Ok(res)
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
    fn test_expression_arithmatic() -> Result<(), Error> {
        let mut dict = HashMap::new();
        let expr = Expression::Expr {
            lhs: Box::new(Expression::Int(10)),
            operator: "+".to_owned(),
            rhs: Box::new(Expression::Int(10)),
        };

        assert_eq!(expr.calculate(&mut dict)?, Expression::Int(20));

        let expr = Expression::Expr {
            lhs: Box::new(Expression::IntU8(10)),
            operator: "-".to_owned(),
            rhs: Box::new(Expression::IntU8(10)),
        };

        assert_eq!(expr.calculate(&mut dict)?, Expression::IntU8(0));

        let expr = Expression::Expr {
            lhs: Box::new(Expression::Int(10)),
            operator: "*".to_owned(),
            rhs: Box::new(Expression::IntU8(10)),
        };

        assert_eq!(expr.calculate(&mut dict)?, Expression::Int(100));

        let expr = Expression::Expr {
            lhs: Box::new(Expression::Int(10)),
            operator: "/".to_owned(),
            rhs: Box::new(Expression::Float(2.0)),
        };

        assert_eq!(expr.calculate(&mut dict)?, Expression::Float(5.0));

        let expr = Expression::Expr {
            lhs: Box::new(Expression::Float(10.0)),
            operator: "%".to_owned(),
            rhs: Box::new(Expression::Float(2.0)),
        };

        assert_eq!(expr.calculate(&mut dict)?, Expression::Float(10.0 % 2.0));

        let expr = Expression::Expr {
            lhs: Box::new(Expression::Int(10)),
            operator: "%".to_owned(),
            rhs: Box::new(Expression::Bool(true)),
        };

        assert_eq!(expr.calculate(&mut dict)?, Expression::Int(10 % 1));

        Ok(())
    }

    #[test]
    fn test_const_expr_name() -> Result<(), Error> {
        let mut dict = HashMap::new();
        dict.insert("DUMP_FLAG_PRIORITY_CRITICAL".to_owned(),
            ConstExpr::Expression(Expression::Expr {
                lhs: Box::new(Expression::Int(1)),
                operator: "<<".to_owned(),
                rhs: Box::new(Expression::Int(0)),
            })
        );

        dict.insert("DUMP_FLAG_PRIORITY_HIGH".to_owned(),
            ConstExpr::Expression(Expression::Expr {
                lhs: Box::new(Expression::Int(1)),
                operator: "<<".to_owned(),
                rhs: Box::new(Expression::Int(1)),
            })
        );

        dict.insert("DUMP_FLAG_PRIORITY_NORMAL".to_owned(),
            ConstExpr::Expression(Expression::Expr {
                lhs: Box::new(Expression::Int(1)),
                operator: "<<".to_owned(),
                rhs: Box::new(Expression::Int(2)),
            })
        );

        dict.insert("DUMP_FLAG_PRIORITY_DEFAULT".to_owned(),
            ConstExpr::Expression(Expression::Expr {
                lhs: Box::new(Expression::Int(1)),
                operator: "<<".to_owned(),
                rhs: Box::new(Expression::Int(3)),
            })
        );


        let expr = Expression::Expr {
            lhs: Box::new(Expression::Name("DUMP_FLAG_PRIORITY_CRITICAL".to_owned())),
            operator: "|".to_owned(),
            rhs: Box::new(Expression::Expr{
                lhs: Box::new(Expression::Name("DUMP_FLAG_PRIORITY_HIGH".to_owned())),
                operator: "|".to_owned(),
                rhs: Box::new(Expression::Expr{
                    lhs: Box::new(Expression::Name("DUMP_FLAG_PRIORITY_NORMAL".to_owned())),
                    operator: "|".to_owned(),
                    rhs: Box::new(Expression::Name("DUMP_FLAG_PRIORITY_DEFAULT".to_owned())),
                })
            }),
        };

        assert_eq!(expr.calculate(&mut dict)?, Expression::Int(15));

        Ok(())
    }
}