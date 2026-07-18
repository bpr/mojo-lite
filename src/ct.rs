//! The shared compile-time value model.
//!
//! `CtValue` is the one representation of a compile-time value across the
//! compiler: the [`comptime`](crate::comptime) elaboration pass builds and folds
//! them (`comptime` constants, `comptime if`/`for`, CTFE), and the
//! [`checker`](crate::checker) uses them for value-parameter arguments
//! (`FixedBuffer[8]`, `SIMD[DType.int32, 4]`). Consolidating the two former
//! representations (comptime's own value enum and the checker's former `CtVal`)
//! here keeps the two phases speaking the same language — a prerequisite for
//! type-valued compile-time members.
//!
//! Scalar values and recursively materializable tuples/lists have a runtime
//! literal form; `Type`, `Reflected`, and `Param` are compile-time-only.

use crate::ast::{Expr, ExprKind};
use crate::token::Span;
use crate::types::Ty;
use std::fmt;

/// A compile-time value. Scalar values drive folding; `Tuple`/`List`
/// let `comptime for` iterate compile-time collections; `Type` carries a
/// semantic type for associated comptime members; `Param` is a symbolic value
/// parameter while a generic body is being checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtValue {
    Int(i64),
    UInt(u64),
    /// IEEE-754 bits provide deterministic equality and specialization keys.
    Float(u64),
    Bool(bool),
    Str(String),
    Tuple(Vec<CtValue>),
    List(Vec<CtValue>),
    Type(Box<Ty>),
    /// The zero-sized compile-time handle produced by current Mojo's
    /// `reflect[T]` API. Field selection returns another handle, allowing
    /// `.field[name]` / `.field_at[index]` chains to terminate in `.T`.
    Reflected(Box<Ty>),
    Param(String),
}

/// A canonical dependent compile-time expression retained in generic metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtExpr {
    Value(CtValue),
    Param(String),
    Neg(Box<CtExpr>),
    Add(Box<CtExpr>, Box<CtExpr>),
    Sub(Box<CtExpr>, Box<CtExpr>),
    Mul(Box<CtExpr>, Box<CtExpr>),
    FloorDiv(Box<CtExpr>, Box<CtExpr>),
    Mod(Box<CtExpr>, Box<CtExpr>),
    Pow(Box<CtExpr>, Box<CtExpr>),
}

impl CtExpr {
    pub fn evaluate(
        &self,
        parameters: &std::collections::HashMap<String, CtValue>,
    ) -> Option<CtValue> {
        use CtExpr::*;
        match self {
            Value(value) => Some(value.clone()),
            Param(name) => parameters.get(name).cloned(),
            Neg(value) => match value.evaluate(parameters)? {
                CtValue::Int(value) => Some(CtValue::Int(-value)),
                _ => None,
            },
            Add(left, right) => match (left.evaluate(parameters)?, right.evaluate(parameters)?) {
                (CtValue::Int(left), CtValue::Int(right)) => Some(CtValue::Int(left + right)),
                (CtValue::Str(left), CtValue::Str(right)) => Some(CtValue::Str(left + &right)),
                _ => None,
            },
            Sub(left, right) => int_binary(left, right, parameters, |a, b| a.checked_sub(b)),
            Mul(left, right) => int_binary(left, right, parameters, |a, b| a.checked_mul(b)),
            FloorDiv(left, right) => {
                int_binary(left, right, parameters, |a, b| a.checked_div_euclid(b))
            }
            Mod(left, right) => int_binary(left, right, parameters, |a, b| a.checked_rem_euclid(b)),
            Pow(left, right) => int_binary(left, right, parameters, |a, b| {
                u32::try_from(b).ok().and_then(|b| a.checked_pow(b))
            }),
        }
    }
}

fn int_binary(
    left: &CtExpr,
    right: &CtExpr,
    parameters: &std::collections::HashMap<String, CtValue>,
    operation: impl FnOnce(i64, i64) -> Option<i64>,
) -> Option<CtValue> {
    match (left.evaluate(parameters)?, right.evaluate(parameters)?) {
        (CtValue::Int(left), CtValue::Int(right)) => operation(left, right).map(CtValue::Int),
        _ => None,
    }
}

impl CtValue {
    /// Materialize this value as a literal expression, or `None` when it has no
    /// runtime form (a symbolic `Param`, or a collection containing one).
    pub fn materialize(&self, span: Span) -> Option<Expr> {
        let kind = match self {
            CtValue::Int(n) => ExprKind::Int(*n),
            CtValue::UInt(n) if *n <= i64::MAX as u64 => ExprKind::Int(*n as i64),
            CtValue::UInt(_) => return None,
            CtValue::Float(bits) => ExprKind::Float(f64::from_bits(*bits)),
            CtValue::Bool(b) => ExprKind::Bool(*b),
            CtValue::Str(s) => ExprKind::Str(s.clone()),
            CtValue::Tuple(vs) => ExprKind::TupleLit(materialize_all(vs, span)?),
            CtValue::List(vs) => ExprKind::ListLit(materialize_all(vs, span)?),
            CtValue::Type(_) | CtValue::Reflected(_) => return None,
            CtValue::Param(_) => return None,
        };
        Some(Expr {
            kind,
            span,
            source: None,
        })
    }
}

fn materialize_all(vs: &[CtValue], span: Span) -> Option<Vec<Expr>> {
    vs.iter().map(|v| v.materialize(span)).collect()
}

impl fmt::Display for CtValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CtValue::Int(n) => write!(f, "{n}"),
            CtValue::UInt(n) => write!(f, "{n}u"),
            CtValue::Float(bits) => write!(f, "{:?}", f64::from_bits(*bits)),
            CtValue::Bool(b) => write!(f, "{b}"),
            CtValue::Str(s) => write!(f, "{s:?}"),
            CtValue::Type(ty) => write!(f, "{ty}"),
            CtValue::Reflected(ty) => write!(f, "reflect[{ty}]"),
            CtValue::Param(name) => write!(f, "{name}"),
            CtValue::Tuple(vs) | CtValue::List(vs) => {
                let (open, close) = match self {
                    CtValue::Tuple(_) => ('(', ')'),
                    _ => ('[', ']'),
                };
                write!(f, "{open}")?;
                for (i, v) in vs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "{close}")
            }
        }
    }
}
