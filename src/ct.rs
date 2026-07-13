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
//! Only `Int`/`Bool`/`Str`/`Tuple`/`List` have a runtime literal form; `Type` and
//! `Param` are compile-time-only values, so they do not materialize.

use crate::ast::{Expr, ExprKind};
use crate::token::Span;
use crate::types::Ty;
use std::fmt;

/// A compile-time value. `Int`/`Bool` drive control flow; `Str`/`Tuple`/`List`
/// let `comptime for` iterate compile-time collections; `Type` carries a
/// semantic type for associated comptime members; `Param` is a symbolic value
/// parameter while a generic body is being checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtValue {
    Int(i64),
    Bool(bool),
    Str(String),
    Tuple(Vec<CtValue>),
    List(Vec<CtValue>),
    Type(Box<Ty>),
    Param(String),
}

impl CtValue {
    /// Materialize this value as a literal expression, or `None` when it has no
    /// runtime form (a symbolic `Param`, or a collection containing one).
    pub fn materialize(&self, span: Span) -> Option<Expr> {
        let kind = match self {
            CtValue::Int(n) => ExprKind::Int(*n),
            CtValue::Bool(b) => ExprKind::Bool(*b),
            CtValue::Str(s) => ExprKind::Str(s.clone()),
            CtValue::Tuple(vs) => ExprKind::TupleLit(materialize_all(vs, span)?),
            CtValue::List(vs) => ExprKind::ListLit(materialize_all(vs, span)?),
            CtValue::Type(_) => return None,
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
            CtValue::Bool(b) => write!(f, "{b}"),
            CtValue::Str(s) => write!(f, "{s:?}"),
            CtValue::Type(ty) => write!(f, "{ty}"),
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
