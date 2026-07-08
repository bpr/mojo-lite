//! Runtime values and their operations, used by the register-VM `backend`. This
//! module owns the `Value` type (`SimdLanes` and all), the scalar/`SIMD`/`List`
//! operations, coercion, and the utility built-ins — the shared value layer the
//! backend consumes.

use std::fmt;

use crate::ast::{Dtype, InfixOp, PrefixOp, Type};
use crate::error::RuntimeError;

/// A runtime value produced by evaluating an expression.
#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    UInt(u64),
    Float64(f64),
    Bool(bool),
    Str(String),
    None,
    /// A half-open integer range `[start, stop)` with the given `step`, produced
    /// by the built-in `range(...)` and consumed by `for`. Not a first-class
    /// value: there is no annotation for it, so it only lives in a `for` header.
    Range {
        start: i64,
        stop: i64,
        step: i64,
    },
    /// A struct instance: its type name, field values (in declaration order), and
    /// any reified value parameters (`FixedBuffer[8]` stores `size = 8`, read via
    /// `Self.size` in methods). A value type — `Clone` deep-copies, giving value
    /// semantics.
    Struct {
        name: String,
        fields: Vec<(String, Value)>,
        value_params: Vec<(String, Value)>,
    },
    /// A SIMD vector: its element type and lane values. The width-1 case is a
    /// scalar-alias value (`Int32`, …).
    Simd {
        dtype: Dtype,
        lanes: SimdLanes,
    },
    /// An `Error` value carrying its message (what `raise` raises).
    Error(String),
    /// A `List` value — a value type (`Clone` deep-copies its elements, so
    /// assigning/passing a list copies it, matching Mojo's value semantics).
    List(Vec<Value>),
    /// A `Tuple` value — a fixed-size, heterogeneous value type (`Clone`
    /// deep-copies; immutable — no element write).
    Tuple(Vec<Value>),
    /// An `UnsafePointer[T]` — a base offset into the VM's type-erased heap arena
    /// (`Option B`: the arena is a `Vec<Value>`). Copying a pointer copies the
    /// offset, so two copies **alias** the same storage (the point of a pointer).
    Pointer(usize),
    /// A **tombstone** left when a value is moved out of a variable slot (`b = a^`)
    /// in the VM backend: the source slot holds `Moved` afterward, so a
    /// use-after-move surfaces as a loud runtime error (a defensive check — the
    /// ownership analysis already rejects it statically). Never produced by the
    /// tree-walker (which copies), and a no-op to drop.
    Moved,
}

/// The lanes of a SIMD value, one representation per element-type kind. Integer
/// lanes are stored as `i128` holding the post-wrap mathematical value (so an
/// unsigned lane is its non-negative value), which makes comparisons correct for
/// both signed and unsigned dtypes.
#[derive(Debug, Clone, PartialEq)]
pub enum SimdLanes {
    Int(Vec<i128>),
    Float(Vec<f64>),
    Bool(Vec<bool>),
}

impl SimdLanes {
    fn width(&self) -> usize {
        match self {
            SimdLanes::Int(v) => v.len(),
            SimdLanes::Float(v) => v.len(),
            SimdLanes::Bool(v) => v.len(),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::UInt(a), Value::UInt(b)) => a == b,
            (Value::Float64(a), Value::Float64(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::None, Value::None) => true,
            // Closures have identity, not structural, equality.
            (
                Value::Range {
                    start: s1,
                    stop: e1,
                    step: t1,
                },
                Value::Range {
                    start: s2,
                    stop: e2,
                    step: t2,
                },
            ) => s1 == s2 && e1 == e2 && t1 == t2,
            (
                Value::Struct {
                    name: n1,
                    fields: f1,
                    value_params: p1,
                },
                Value::Struct {
                    name: n2,
                    fields: f2,
                    value_params: p2,
                },
            ) => n1 == n2 && f1 == f2 && p1 == p2,
            (
                Value::Simd {
                    dtype: d1,
                    lanes: l1,
                },
                Value::Simd {
                    dtype: d2,
                    lanes: l2,
                },
            ) => d1 == d2 && l1 == l2,
            (Value::Error(a), Value::Error(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Tuple(a), Value::Tuple(b)) => a == b,
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{}", n),
            Value::UInt(n) => write!(f, "{}", n),
            // `{:?}` keeps the decimal point (e.g. `3.0`), distinguishing from Int.
            Value::Float64(x) => write!(f, "{:?}", x),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Str(s) => write!(f, "{}", s),
            Value::None => write!(f, "None"),
            Value::Range { start, stop, step } => write!(f, "range({}, {}, {})", start, stop, step),
            Value::Struct {
                name,
                fields,
                value_params,
            } => {
                write!(f, "{}", name)?;
                if !value_params.is_empty() {
                    write!(f, "[")?;
                    for (i, (_, val)) in value_params.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", val)?;
                    }
                    write!(f, "]")?;
                }
                write!(f, "(")?;
                for (i, (fname, val)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}={}", fname, val)?;
                }
                write!(f, ")")
            }
            // A width-1 SIMD prints as the bare lane; wider as `[l0, l1, ...]`.
            Value::Simd { lanes, .. } => {
                let strs: Vec<String> = match lanes {
                    SimdLanes::Int(v) => v.iter().map(|n| n.to_string()).collect(),
                    SimdLanes::Float(v) => v.iter().map(|x| format!("{:?}", x)).collect(),
                    SimdLanes::Bool(v) => v.iter().map(|b| b.to_string()).collect(),
                };
                if strs.len() == 1 {
                    write!(f, "{}", strs[0])
                } else {
                    write!(f, "[{}]", strs.join(", "))
                }
            }
            Value::Error(msg) => write!(f, "Error({:?})", msg),
            Value::Pointer(base) => write!(f, "UnsafePointer(0x{:x})", base),
            Value::Moved => write!(f, "<moved>"),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            Value::Tuple(items) => {
                write!(f, "(")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                // A 1-tuple prints with a trailing comma, like Python/Mojo.
                if items.len() == 1 {
                    write!(f, ",")?;
                }
                write!(f, ")")
            }
        }
    }
}

/// Classify a `[...]` parameter list into `(name, is_value)` pairs, mirroring the
/// checker: a lone bound naming a scalar type marks a value parameter. Runtime
/// only needs value parameters (type parameters are erased), but the full order
/// is kept so explicit `[...]` arguments line up positionally.
pub(crate) fn classify_param_decls(type_params: &[crate::ast::TypeParam]) -> Vec<(String, bool)> {
    type_params
        .iter()
        .map(|tp| {
            let is_value = matches!(
                tp.bounds.as_slice(),
                [only] if matches!(only.as_str(), "Int" | "UInt" | "Bool" | "String" | "Float64")
            );
            (tp.name.clone(), is_value)
        })
        .collect()
}

/// The Mojo type name of a runtime value, for error messages.
pub(crate) fn type_name(value: &Value) -> String {
    match value {
        Value::Int(_) => "Int".to_string(),
        Value::UInt(_) => "UInt".to_string(),
        Value::Float64(_) => "Float64".to_string(),
        Value::Bool(_) => "Bool".to_string(),
        Value::Str(_) => "String".to_string(),
        Value::None => "None".to_string(),
        Value::Range { .. } => "range".to_string(),
        Value::Struct { name, .. } => name.clone(),
        Value::Simd { dtype, lanes } => {
            if lanes.width() == 1 {
                dtype.scalar_alias().unwrap_or("SIMD").to_string()
            } else {
                format!("SIMD[DType.{}, {}]", dtype.name(), lanes.width())
            }
        }
        Value::Error(_) => "Error".to_string(),
        Value::Pointer(_) => "UnsafePointer".to_string(),
        Value::Moved => "<moved>".to_string(),
        Value::List(_) => "List".to_string(),
        Value::Tuple(items) => {
            let elems: Vec<String> = items.iter().map(type_name).collect();
            format!("Tuple[{}]", elems.join(", "))
        }
    }
}

/// Structural equality for `==`/`!=`. Defined only between values of the same
/// scalar type; comparing across types (or closures) is a type error.
pub(crate) fn values_equal(a: &Value, b: &Value) -> Result<bool, RuntimeError> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x == y),
        (Value::UInt(x), Value::UInt(y)) => Ok(x == y),
        (Value::Float64(x), Value::Float64(y)) => Ok(x == y),
        (Value::Bool(x), Value::Bool(y)) => Ok(x == y),
        (Value::Str(x), Value::Str(y)) => Ok(x == y),
        (Value::None, Value::None) => Ok(true),
        _ => Err(RuntimeError::TypeError(format!(
            "cannot compare {} and {}",
            type_name(a),
            type_name(b)
        ))),
    }
}

/// A numeric value lifted out of `Value` for arithmetic. Operands are promoted to
/// a common kind before an operator is applied (`Int < UInt < Float64`).
#[derive(Clone, Copy)]
enum Num {
    I(i64),
    U(u64),
    F(f64),
}

impl Num {
    fn rank(self) -> u8 {
        match self {
            Num::I(_) => 0,
            Num::U(_) => 1,
            Num::F(_) => 2,
        }
    }
    fn as_i64(self) -> i64 {
        match self {
            Num::I(n) => n,
            Num::U(n) => n as i64,
            Num::F(x) => x as i64,
        }
    }
    fn as_u64(self) -> u64 {
        match self {
            Num::I(n) => n as u64,
            Num::U(n) => n,
            Num::F(x) => x as u64,
        }
    }
    fn as_f64(self) -> f64 {
        match self {
            Num::I(n) => n as f64,
            Num::U(n) => n as f64,
            Num::F(x) => x,
        }
    }
}

/// View a value as a number, if it is one.
fn as_num(v: &Value) -> Option<Num> {
    match v {
        Value::Int(n) => Some(Num::I(*n)),
        Value::UInt(n) => Some(Num::U(*n)),
        Value::Float64(x) => Some(Num::F(*x)),
        _ => None,
    }
}

/// Apply a (strict) binary operator to two already-evaluated values — the shared
/// Apply a unary operator to an already-evaluated operand. Shared by the tree
/// evaluator's `eval_prefix` and the VM backend, so the two agree on semantics.
pub(crate) fn apply_prefix(op: PrefixOp, value: Value) -> Result<Value, RuntimeError> {
    match (op, value) {
        (PrefixOp::Neg, Value::Int(n)) => Ok(Value::Int(-n)),
        (PrefixOp::Neg, Value::Float64(x)) => Ok(Value::Float64(-x)),
        (PrefixOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (PrefixOp::Neg, v) => Err(RuntimeError::TypeError(format!(
            "cannot negate {}",
            type_name(&v)
        ))),
        (PrefixOp::Not, v) => Err(RuntimeError::TypeError(format!(
            "'not' requires Bool, got {}",
            type_name(&v)
        ))),
    }
}

/// core of `eval_infix` and augmented assignment. `and`/`or` short-circuit and so
/// are handled by the caller, not here.
pub(crate) fn apply_infix(op: InfixOp, l: Value, r: Value) -> Result<Value, RuntimeError> {
    // Membership `in` / `not in`.
    if matches!(op, InfixOp::In | InfixOp::NotIn) {
        return eval_membership(op, &l, &r);
    }
    // Elementwise SIMD operators (before the scalar-numeric path).
    if matches!(l, Value::Simd { .. }) || matches!(r, Value::Simd { .. }) {
        return simd_binop(op, &l, &r);
    }
    // `+` on two strings concatenates.
    if let (InfixOp::Add, Value::Str(a), Value::Str(b)) = (op, &l, &r) {
        return Ok(Value::Str(format!("{}{}", a, b)));
    }
    // Numeric operators (arithmetic, ordering, and numeric equality), with
    // promotion of mixed numeric kinds.
    if let (Some(a), Some(b)) = (as_num(&l), as_num(&r)) {
        return numeric_op(op, a, b);
    }
    // Equality between non-numeric values (Bool/String/None).
    match op {
        InfixOp::Eq => Ok(Value::Bool(values_equal(&l, &r)?)),
        InfixOp::Ne => Ok(Value::Bool(!values_equal(&l, &r)?)),
        _ => Err(RuntimeError::TypeError(format!(
            "operator {:?} is not defined for {} and {}",
            op,
            type_name(&l),
            type_name(&r)
        ))),
    }
}

/// Apply a numeric binary operator, promoting operands to a common kind. The
/// checker rejects mixing two *concrete* numeric types, so any kind difference
/// here comes from a materialized literal — promotion resolves it soundly.
fn numeric_op(op: InfixOp, a: Num, b: Num) -> Result<Value, RuntimeError> {
    // True division always computes in f64 and yields Float64.
    if op == InfixOp::Div {
        return Ok(Value::Float64(a.as_f64() / b.as_f64()));
    }
    match a.rank().max(b.rank()) {
        2 => float_op(op, a.as_f64(), b.as_f64()),
        1 => uint_op(op, a.as_u64(), b.as_u64()),
        _ => int_op(op, a.as_i64(), b.as_i64()),
    }
}

fn int_op(op: InfixOp, x: i64, y: i64) -> Result<Value, RuntimeError> {
    use InfixOp::*;
    Ok(match op {
        Add => Value::Int(x + y),
        Sub => Value::Int(x - y),
        Mul => Value::Int(x * y),
        FloorDiv => Value::Int(floor_div(x, nonzero(y)?)),
        Mod => Value::Int(floor_mod(x, nonzero(y)?)),
        Pow => Value::Int(x.pow(pow_exp(y)?)),
        Lt => Value::Bool(x < y),
        Gt => Value::Bool(x > y),
        Le => Value::Bool(x <= y),
        Ge => Value::Bool(x >= y),
        Eq => Value::Bool(x == y),
        Ne => Value::Bool(x != y),
        Div | And | Or | In | NotIn => unreachable!("handled before numeric dispatch"),
    })
}

fn uint_op(op: InfixOp, x: u64, y: u64) -> Result<Value, RuntimeError> {
    use InfixOp::*;
    Ok(match op {
        Add => Value::UInt(x + y),
        Sub => Value::UInt(x - y),
        Mul => Value::UInt(x * y),
        // Unsigned: floor division/modulo are plain `/` and `%`.
        FloorDiv => Value::UInt(x / nonzero_u(y)?),
        Mod => Value::UInt(x % nonzero_u(y)?),
        Pow => Value::UInt(x.pow(pow_exp(y as i64)?)),
        Lt => Value::Bool(x < y),
        Gt => Value::Bool(x > y),
        Le => Value::Bool(x <= y),
        Ge => Value::Bool(x >= y),
        Eq => Value::Bool(x == y),
        Ne => Value::Bool(x != y),
        Div | And | Or | In | NotIn => unreachable!("handled before numeric dispatch"),
    })
}

fn float_op(op: InfixOp, x: f64, y: f64) -> Result<Value, RuntimeError> {
    use InfixOp::*;
    Ok(match op {
        Add => Value::Float64(x + y),
        Sub => Value::Float64(x - y),
        Mul => Value::Float64(x * y),
        FloorDiv => Value::Float64((x / y).floor()),
        Mod => Value::Float64(x - y * (x / y).floor()),
        Pow => Value::Float64(x.powf(y)),
        Lt => Value::Bool(x < y),
        Gt => Value::Bool(x > y),
        Le => Value::Bool(x <= y),
        Ge => Value::Bool(x >= y),
        Eq => Value::Bool(x == y),
        Ne => Value::Bool(x != y),
        Div | And | Or | In | NotIn => unreachable!("handled before numeric dispatch"),
    })
}

fn nonzero(y: i64) -> Result<i64, RuntimeError> {
    (y != 0)
        .then_some(y)
        .ok_or_else(|| RuntimeError::TypeError("integer division or modulo by zero".to_string()))
}

fn nonzero_u(y: u64) -> Result<u64, RuntimeError> {
    (y != 0)
        .then_some(y)
        .ok_or_else(|| RuntimeError::TypeError("integer division or modulo by zero".to_string()))
}

/// Validate an integer exponent for `**` (non-negative and `u32`-sized).
fn pow_exp(y: i64) -> Result<u32, RuntimeError> {
    u32::try_from(y).map_err(|_| {
        RuntimeError::TypeError(
            "'**' exponent must be a non-negative Int that fits in 32 bits".to_string(),
        )
    })
}

/// Python/Mojo floor division: round toward negative infinity.
fn floor_div(x: i64, y: i64) -> i64 {
    let q = x / y;
    let r = x % y;
    if r != 0 && ((r < 0) != (y < 0)) {
        q - 1
    } else {
        q
    }
}

/// Python/Mojo modulo: the result takes the sign of the divisor.
fn floor_mod(x: i64, y: i64) -> i64 {
    let r = x % y;
    if r != 0 && ((r < 0) != (y < 0)) {
        r + y
    } else {
        r
    }
}

// --- Collections / indexing ---

/// Convert a signed index into a bounds-checked `usize`, or a runtime error.
pub(crate) fn bounds_check(idx: i64, len: usize, what: &str) -> Result<usize, RuntimeError> {
    if idx < 0 || idx as usize >= len {
        return Err(RuntimeError::TypeError(format!(
            "{} index {} out of range 0..{}",
            what, idx, len
        )));
    }
    Ok(idx as usize)
}

/// A `Value` used as an index, requiring it to be an `Int`.
pub(crate) fn value_as_index(v: &Value) -> Result<i64, RuntimeError> {
    match v {
        Value::Int(n) => Ok(*n),
        other => Err(RuntimeError::TypeError(format!(
            "index must be Int, got {}",
            type_name(other)
        ))),
    }
}

/// The concrete indices a slice `[lower:upper:step]` selects from a sequence of
/// `len` elements, using Python semantics (negative indices wrap; bounds clamp;
/// `step` may be negative to reverse; `None` bounds take direction-aware defaults).
/// A zero `step` is a runtime error.
pub(crate) fn slice_indices(
    len: i64,
    lower: Option<i64>,
    upper: Option<i64>,
    step: Option<i64>,
) -> Result<Vec<usize>, RuntimeError> {
    let step = step.unwrap_or(1);
    if step == 0 {
        return Err(RuntimeError::TypeError("slice step cannot be zero".to_string()));
    }
    // Clamp an explicit bound to a valid range, wrapping a negative index once.
    let adjust = |i: i64| -> i64 {
        let i = if i < 0 { i + len } else { i };
        if step > 0 {
            i.clamp(0, len)
        } else {
            i.clamp(-1, len - 1)
        }
    };
    let (start, stop) = if step > 0 {
        (lower.map_or(0, adjust), upper.map_or(len, adjust))
    } else {
        (lower.map_or(len - 1, adjust), upper.map_or(-1, adjust))
    };
    let mut idxs = Vec::new();
    let mut i = start;
    if step > 0 {
        while i < stop {
            idxs.push(i as usize);
            i += step;
        }
    } else {
        while i > stop {
            idxs.push(i as usize);
            i += step;
        }
    }
    Ok(idxs)
}

/// Slice a `List`/`String` value (`a[lower:upper:step]`), returning a new value of
/// the same kind. `String` is sliced over its **bytes** (consistent with `len`),
/// rebuilt lossily if a multibyte boundary is split.
pub(crate) fn slice_value(
    v: &Value,
    lower: Option<i64>,
    upper: Option<i64>,
    step: Option<i64>,
) -> Result<Value, RuntimeError> {
    match v {
        Value::List(items) => {
            let idxs = slice_indices(items.len() as i64, lower, upper, step)?;
            Ok(Value::List(idxs.into_iter().map(|i| items[i].clone()).collect()))
        }
        Value::Str(s) => {
            let bytes = s.as_bytes();
            let idxs = slice_indices(bytes.len() as i64, lower, upper, step)?;
            let picked: Vec<u8> = idxs.into_iter().map(|i| bytes[i]).collect();
            Ok(Value::Str(String::from_utf8_lossy(&picked).into_owned()))
        }
        other => Err(RuntimeError::TypeError(format!(
            "cannot slice {}",
            type_name(other)
        ))),
    }
}

/// Whether a `List` method mutates the list (vs. the read-only queries).
pub(crate) fn is_list_mutator(method: &str) -> bool {
    matches!(
        method,
        "append" | "insert" | "remove" | "pop" | "clear" | "reverse" | "extend"
    )
}

/// Apply a `List` method to an owned (mutable) list. Mutating methods change
/// `items`; the query methods (`count`/`index`) delegate to [`list_query`]. An
/// incoming element is coerced to the existing elements' numeric kind.
pub(crate) fn apply_list_method(
    items: &mut Vec<Value>,
    method: &str,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let first = items.first().cloned();
    let coerce_elem = |v: Value| match &first {
        Some(f) => coerce_like(v, f),
        None => v,
    };
    match method {
        "append" => {
            items.push(coerce_elem(args[0].clone()));
            Ok(Value::None)
        }
        "insert" => {
            let i = value_as_index(&args[0])?;
            if i < 0 || i as usize > items.len() {
                return Err(RuntimeError::TypeError(format!(
                    "insert index {} out of range 0..={}",
                    i,
                    items.len()
                )));
            }
            items.insert(i as usize, coerce_elem(args[1].clone()));
            Ok(Value::None)
        }
        "remove" => {
            let target = coerce_elem(args[0].clone());
            match items
                .iter()
                .position(|it| values_equal(it, &target).unwrap_or(false))
            {
                Some(p) => {
                    items.remove(p);
                    Ok(Value::None)
                }
                None => Err(RuntimeError::TypeError(
                    "remove(x): x is not in the list".to_string(),
                )),
            }
        }
        "pop" => {
            let i = if let Some(a) = args.first() {
                bounds_check(value_as_index(a)?, items.len(), "list")?
            } else if items.is_empty() {
                return Err(RuntimeError::TypeError(
                    "pop() from an empty list".to_string(),
                ));
            } else {
                items.len() - 1
            };
            Ok(items.remove(i))
        }
        "clear" => {
            items.clear();
            Ok(Value::None)
        }
        "reverse" => {
            items.reverse();
            Ok(Value::None)
        }
        "extend" => {
            if let Value::List(other) = &args[0] {
                items.extend(other.iter().cloned());
                Ok(Value::None)
            } else {
                Err(RuntimeError::TypeError(format!(
                    "extend() expects a List, got {}",
                    type_name(&args[0])
                )))
            }
        }
        "count" | "index" => list_query(items, method, args),
        _ => Err(RuntimeError::TypeError(format!(
            "List has no method '{}'",
            method
        ))),
    }
}

/// A read-only `List` query: `count(x)` → the number of equal elements;
/// `index(x)` → the first equal element's index (a runtime error if absent).
pub(crate) fn list_query(items: &[Value], method: &str, args: &[Value]) -> Result<Value, RuntimeError> {
    let target = match items.first() {
        Some(f) => coerce_like(args[0].clone(), f),
        None => args[0].clone(),
    };
    let eq = |it: &Value| values_equal(it, &target).unwrap_or(false);
    match method {
        "count" => Ok(Value::Int(items.iter().filter(|it| eq(it)).count() as i64)),
        "index" => match items.iter().position(eq) {
            Some(p) => Ok(Value::Int(p as i64)),
            None => Err(RuntimeError::TypeError(
                "index(x): x is not in the list".to_string(),
            )),
        },
        _ => Err(RuntimeError::TypeError(format!(
            "List has no method '{}'",
            method
        ))),
    }
}

/// Evaluate `x in c` / `x not in c` → `Bool`. `c` is a `List` (element
/// membership, comparing the coerced value) or a `String` (substring test).
pub(crate) fn eval_membership(op: InfixOp, l: &Value, r: &Value) -> Result<Value, RuntimeError> {
    let found = match r {
        Value::List(items) => {
            let target = match items.first() {
                Some(f) => coerce_like(l.clone(), f),
                None => l.clone(),
            };
            items
                .iter()
                .any(|it| values_equal(it, &target).unwrap_or(false))
        }
        Value::Str(s) => match l {
            Value::Str(sub) => s.contains(sub.as_str()),
            other => {
                return Err(RuntimeError::TypeError(format!(
                    "left operand of 'in' on a String must be a String, got {}",
                    type_name(other)
                )));
            }
        },
        other => {
            return Err(RuntimeError::TypeError(format!(
                "'in' requires a List or String, got {}",
                type_name(other)
            )));
        }
    };
    let result = if op == InfixOp::NotIn { !found } else { found };
    Ok(Value::Bool(result))
}

/// If every element is numeric, promote them all to a common kind
/// (`Int < UInt < Float64`), so a mixed-literal list like `[1, 2.0]` becomes
/// uniform `Float64`. A list with any non-numeric element is left unchanged.
pub(crate) fn promote_numeric_elems(items: &mut [Value]) {
    let Some(nums): Option<Vec<Num>> = items.iter().map(as_num).collect() else {
        return; // some element is non-numeric
    };
    let Some(rank) = nums.iter().map(|n| n.rank()).max() else {
        return; // empty
    };
    for (item, n) in items.iter_mut().zip(nums) {
        *item = match rank {
            2 => Value::Float64(n.as_f64()),
            1 => Value::UInt(n.as_u64()),
            _ => Value::Int(n.as_i64()),
        };
    }
}

// --- SIMD ---

/// Wrap an integer to a dtype's bit width (bit-accurate overflow), storing the
/// post-wrap mathematical value (non-negative for unsigned dtypes).
fn wrap(dtype: Dtype, v: i128) -> i128 {
    match dtype {
        Dtype::Int8 => v as i8 as i128,
        Dtype::Int16 => v as i16 as i128,
        Dtype::Int32 => v as i32 as i128,
        Dtype::Int64 => v as i64 as i128,
        Dtype::UInt8 => v as u8 as i128,
        Dtype::UInt16 => v as u16 as i128,
        Dtype::UInt32 => v as u32 as i128,
        Dtype::UInt64 => v as u64 as i128,
        Dtype::Float32 | Dtype::Float64 | Dtype::Bool => v,
    }
}

/// Round an `f64` to single precision (for `float32` lanes).
fn round_f32(x: f64) -> f64 {
    x as f32 as f64
}

/// Round a float result to its lane precision: `float32` truncates to single
/// precision, `float64` keeps full `f64`. (Called only for float dtypes.)
fn round_lane(dtype: Dtype, x: f64) -> f64 {
    match dtype {
        Dtype::Float32 => round_f32(x),
        _ => x,
    }
}

/// Build a SIMD `Value` from `dtype`+`lanes`, canonicalizing a **width-1
/// `float64`** to the native `Value::Float64` — the runtime side of unifying
/// `Float64` with `SIMD[DType.float64, 1]`.
fn simd_value(dtype: Dtype, lanes: SimdLanes) -> Value {
    if dtype == Dtype::Float64
        && let SimdLanes::Float(v) = &lanes
        && v.len() == 1
    {
        return Value::Float64(v[0]);
    }
    Value::Simd { dtype, lanes }
}

/// A scalar value's integer content, for building an integer SIMD lane.
fn value_to_int(v: &Value) -> Result<i128, RuntimeError> {
    match v {
        Value::Int(n) => Ok(*n as i128),
        Value::UInt(n) => Ok(*n as i128),
        Value::Bool(b) => Ok(*b as i128),
        Value::Simd {
            lanes: SimdLanes::Int(l),
            ..
        } if l.len() == 1 => Ok(l[0]),
        other => Err(RuntimeError::TypeError(format!(
            "cannot use {} as an integer SIMD element",
            type_name(other)
        ))),
    }
}

/// A scalar value's floating content, for building a float SIMD lane.
fn value_to_float(v: &Value) -> Result<f64, RuntimeError> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Float64(x) => Ok(*x),
        Value::Simd {
            lanes: SimdLanes::Float(l),
            ..
        } if l.len() == 1 => Ok(l[0]),
        other => Err(RuntimeError::TypeError(format!(
            "cannot use {} as a float SIMD element",
            type_name(other)
        ))),
    }
}

// --- Shared numeric/utility built-ins (value level) -------------------------
//
// These operate on already-evaluated `Value`s so both the tree-walker
// (`eval_*`, which evaluate their argument expressions first) and the VM backend
// (`call_named`, which has the values in registers) apply identical semantics —
// no second copy of the rules. The checker guarantees arity/typing, so these are
// the runtime realization only.

/// `abs(x)`: absolute value of a numeric (`UInt` is already non-negative).
pub(crate) fn builtin_abs(v: Value) -> Result<Value, RuntimeError> {
    match v {
        Value::Int(n) => Ok(Value::Int(n.wrapping_abs())),
        Value::Float64(x) => Ok(Value::Float64(x.abs())),
        Value::UInt(n) => Ok(Value::UInt(n)),
        other => Err(RuntimeError::TypeError(format!(
            "abs() expects a numeric value, got {}",
            type_name(&other)
        ))),
    }
}

/// `min(a, b)` / `max(a, b)`: promote the operands to a common numeric kind (as
/// arithmetic does) and return the smaller/larger, rebuilt in that kind.
pub(crate) fn builtin_min_max(is_min: bool, a: Value, b: Value) -> Result<Value, RuntimeError> {
    let (na, nb) = match (as_num(&a), as_num(&b)) {
        (Some(na), Some(nb)) => (na, nb),
        _ => {
            return Err(RuntimeError::TypeError(format!(
                "min()/max() expect numeric values, got {} and {}",
                type_name(&a),
                type_name(&b)
            )));
        }
    };
    Ok(match na.rank().max(nb.rank()) {
        2 => {
            let (x, y) = (na.as_f64(), nb.as_f64());
            Value::Float64(if (x <= y) == is_min { x } else { y })
        }
        1 => {
            let (x, y) = (na.as_u64(), nb.as_u64());
            Value::UInt(if (x <= y) == is_min { x } else { y })
        }
        _ => {
            let (x, y) = (na.as_i64(), nb.as_i64());
            Value::Int(if (x <= y) == is_min { x } else { y })
        }
    })
}

/// `round(x)`: nearest `Float64` (ties away from zero, via `f64::round`).
pub(crate) fn builtin_round(v: Value) -> Result<Value, RuntimeError> {
    Ok(Value::Float64(value_to_float(&v)?.round()))
}

/// `Int(x)` / `UInt(x)` / `Float64(x)`: convert one numeric-or-`Bool` value
/// between concrete numeric types (`Float64`→integer truncates toward zero,
/// `Bool` is 0/1), following Mojo.
pub(crate) fn builtin_convert(name: &str, v: Value) -> Result<Value, RuntimeError> {
    let (as_i, as_u, as_f) = match v {
        Value::Int(n) => (n, n as u64, n as f64),
        Value::UInt(n) => (n as i64, n, n as f64),
        Value::Float64(x) => (x as i64, x as u64, x),
        Value::Bool(b) => (b as i64, b as u64, if b { 1.0 } else { 0.0 }),
        other => {
            return Err(RuntimeError::TypeError(format!(
                "{}() expects a numeric or Bool value, got {}",
                name,
                type_name(&other)
            )));
        }
    };
    Ok(match name {
        "Int" => Value::Int(as_i),
        "UInt" => Value::UInt(as_u),
        _ => Value::Float64(as_f), // "Float64"
    })
}

/// `Error(msg)`: wrap a `String` into an `Error` value.
pub(crate) fn builtin_error(v: Value) -> Result<Value, RuntimeError> {
    match v {
        Value::Str(s) => Ok(Value::Error(s)),
        other => Err(RuntimeError::TypeError(format!(
            "Error() expects a String, got {}",
            type_name(&other)
        ))),
    }
}

/// A scalar value's boolean content, for building a `bool` SIMD lane.
fn value_to_bool_lane(v: &Value) -> Result<bool, RuntimeError> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Simd {
            lanes: SimdLanes::Bool(l),
            ..
        } if l.len() == 1 => Ok(l[0]),
        other => Err(RuntimeError::TypeError(format!(
            "cannot use {} as a bool SIMD element",
            type_name(other)
        ))),
    }
}

/// Read lane `i` of a SIMD as a width-1 SIMD (scalar) of the same dtype — the
/// shared core of `v[i]` reads and SIMD-lane read-modify-write.
/// Build a SIMD value of `dtype`/`width` from element `values`: exactly `width`
/// elements (one per lane) or a single element splatted to every lane. Shared by
/// the tree evaluator and the VM's `MakeSimd`.
pub(crate) fn simd_from_values(
    dtype: Dtype,
    width: usize,
    values: &[Value],
) -> Result<Value, RuntimeError> {
    // A single argument splats; otherwise one argument per lane.
    let pick = |i: usize| &values[if values.len() == 1 { 0 } else { i }];
    let lanes = if dtype == Dtype::Bool {
        let mut v = Vec::with_capacity(width);
        for i in 0..width {
            v.push(value_to_bool_lane(pick(i))?);
        }
        SimdLanes::Bool(v)
    } else if dtype.is_float() {
        let mut v = Vec::with_capacity(width);
        for i in 0..width {
            v.push(round_lane(dtype, value_to_float(pick(i))?));
        }
        SimdLanes::Float(v)
    } else {
        let mut v = Vec::with_capacity(width);
        for i in 0..width {
            v.push(wrap(dtype, value_to_int(pick(i))?));
        }
        SimdLanes::Int(v)
    };
    Ok(simd_value(dtype, lanes))
}

pub(crate) fn read_simd_lane(dtype: Dtype, lanes: &SimdLanes, i: i64) -> Result<Value, RuntimeError> {
    let idx = bounds_check(i, lanes.width(), "SIMD lane")?;
    let lane = match lanes {
        SimdLanes::Int(v) => SimdLanes::Int(vec![v[idx]]),
        SimdLanes::Float(v) => SimdLanes::Float(vec![v[idx]]),
        SimdLanes::Bool(v) => SimdLanes::Bool(vec![v[idx]]),
    };
    Ok(simd_value(dtype, lane))
}

/// Write scalar `value` (or a splatting literal) into lane `i`, wrapping to the
/// element width exactly as construction does (`wrap`/`round_f32`).
pub(crate) fn set_simd_lane(
    dtype: Dtype,
    lanes: &mut SimdLanes,
    i: i64,
    value: Value,
) -> Result<(), RuntimeError> {
    let idx = bounds_check(i, lanes.width(), "SIMD lane")?;
    match lanes {
        SimdLanes::Int(v) => v[idx] = wrap(dtype, value_to_int(&value)?),
        SimdLanes::Float(v) => v[idx] = round_lane(dtype, value_to_float(&value)?),
        SimdLanes::Bool(v) => v[idx] = value_to_bool_lane(&value)?,
    }
    Ok(())
}

/// The `(dtype, width)` of a value if it is a SIMD.
fn simd_shape(v: &Value) -> Option<(Dtype, usize)> {
    match v {
        Value::Simd { dtype, lanes } => Some((*dtype, lanes.width())),
        _ => None,
    }
}

/// Apply an elementwise SIMD operator. One operand may be a scalar that splats
/// to the other's dtype and width. Integer arithmetic is bit-accurate;
/// `float32` rounds each result to single precision. Comparisons yield a `bool`
/// mask. (The checker guarantees dtype/width agreement and operator validity.)
pub(crate) fn simd_binop(op: InfixOp, l: &Value, r: &Value) -> Result<Value, RuntimeError> {
    use InfixOp::*;
    let (dtype, width) = simd_shape(l)
        .or_else(|| simd_shape(r))
        .expect("a SIMD operand");
    let is_cmp = matches!(op, Eq | Ne | Lt | Gt | Le | Ge);
    match dtype {
        Dtype::Bool => {
            let xs = to_bool_lanes(l, width)?;
            let ys = to_bool_lanes(r, width)?;
            let out: Vec<bool> = xs
                .iter()
                .zip(&ys)
                .map(|(a, b)| match op {
                    Eq => a == b,
                    Ne => a != b,
                    _ => false, // checker rejects other ops on bool lanes
                })
                .collect();
            Ok(Value::Simd {
                dtype: Dtype::Bool,
                lanes: SimdLanes::Bool(out),
            })
        }
        d if d.is_float() => {
            let xs = to_float_lanes(l, dtype, width)?;
            let ys = to_float_lanes(r, dtype, width)?;
            if is_cmp {
                let out = xs
                    .iter()
                    .zip(&ys)
                    .map(|(a, b)| float_cmp(op, *a, *b))
                    .collect();
                Ok(Value::Simd {
                    dtype: Dtype::Bool,
                    lanes: SimdLanes::Bool(out),
                })
            } else {
                let out: Vec<f64> = xs
                    .iter()
                    .zip(&ys)
                    .map(|(a, b)| round_lane(dtype, float_arith(op, *a, *b)))
                    .collect();
                Ok(Value::Simd {
                    dtype,
                    lanes: SimdLanes::Float(out),
                })
            }
        }
        d => {
            let xs = to_int_lanes(l, d, width)?;
            let ys = to_int_lanes(r, d, width)?;
            if is_cmp {
                let out = xs
                    .iter()
                    .zip(&ys)
                    .map(|(a, b)| int_cmp(op, *a, *b))
                    .collect();
                Ok(Value::Simd {
                    dtype: Dtype::Bool,
                    lanes: SimdLanes::Bool(out),
                })
            } else {
                let out: Vec<i128> = xs
                    .iter()
                    .zip(&ys)
                    .map(|(a, b)| wrap(d, int_arith(op, *a, *b)))
                    .collect();
                Ok(Value::Simd {
                    dtype: d,
                    lanes: SimdLanes::Int(out),
                })
            }
        }
    }
}

fn int_arith(op: InfixOp, a: i128, b: i128) -> i128 {
    match op {
        InfixOp::Add => a + b,
        InfixOp::Sub => a - b,
        InfixOp::Mul => a * b,
        _ => 0, // checker rejects other arithmetic on SIMD ints
    }
}

fn int_cmp(op: InfixOp, a: i128, b: i128) -> bool {
    match op {
        InfixOp::Eq => a == b,
        InfixOp::Ne => a != b,
        InfixOp::Lt => a < b,
        InfixOp::Gt => a > b,
        InfixOp::Le => a <= b,
        InfixOp::Ge => a >= b,
        _ => false,
    }
}

fn float_arith(op: InfixOp, a: f64, b: f64) -> f64 {
    match op {
        InfixOp::Add => a + b,
        InfixOp::Sub => a - b,
        InfixOp::Mul => a * b,
        InfixOp::Div => a / b,
        _ => 0.0,
    }
}

fn float_cmp(op: InfixOp, a: f64, b: f64) -> bool {
    match op {
        InfixOp::Eq => a == b,
        InfixOp::Ne => a != b,
        InfixOp::Lt => a < b,
        InfixOp::Gt => a > b,
        InfixOp::Le => a <= b,
        InfixOp::Ge => a >= b,
        _ => false,
    }
}

/// Materialize an operand as `width` integer lanes of `dtype` — its own lanes if
/// it is a SIMD, else the scalar splatted across all lanes.
fn to_int_lanes(v: &Value, dtype: Dtype, width: usize) -> Result<Vec<i128>, RuntimeError> {
    match v {
        Value::Simd {
            lanes: SimdLanes::Int(l),
            ..
        } => Ok(l.clone()),
        scalar => Ok(vec![wrap(dtype, value_to_int(scalar)?); width]),
    }
}

fn to_float_lanes(v: &Value, dtype: Dtype, width: usize) -> Result<Vec<f64>, RuntimeError> {
    match v {
        Value::Simd {
            lanes: SimdLanes::Float(l),
            ..
        } => Ok(l.clone()),
        scalar => Ok(vec![round_lane(dtype, value_to_float(scalar)?); width]),
    }
}

fn to_bool_lanes(v: &Value, width: usize) -> Result<Vec<bool>, RuntimeError> {
    match v {
        Value::Simd {
            lanes: SimdLanes::Bool(l),
            ..
        } => Ok(l.clone()),
        scalar => Ok(vec![value_to_bool_lane(scalar)?; width]),
    }
}

/// Coerce a value to a declared type at a binding site. Only materializes a
/// numeric literal's default (`Int`/`Float64`) into another numeric type; all
/// other values pass through unchanged.
pub(crate) fn coerce(value: Value, ty: &Type) -> Value {
    use crate::ast::{Expr, ExprKind, ParamArg};
    match ty {
        Type::UInt => match value {
            Value::Int(n) => Value::UInt(n as u64),
            v => v,
        },
        Type::Float64 => match value {
            Value::Int(n) => Value::Float64(n as f64),
            Value::UInt(n) => Value::Float64(n as f64),
            v => v,
        },
        // `Tuple[...]`: coerce each element to its annotated element type.
        Type::Named(name, args) if name == "Tuple" => match value {
            Value::Tuple(items) => Value::Tuple(
                items
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| match args.get(i) {
                        Some(ParamArg::Type(t)) => coerce(v, t),
                        Some(ParamArg::Value(Expr { kind: ExprKind::Identifier(id), .. })) => {
                            coerce(v, &Type::Named(id.clone(), Vec::new()))
                        }
                        _ => v,
                    })
                    .collect(),
            ),
            v => v,
        },
        _ => value,
    }
}

/// Coerce a value to match an existing binding's numeric type (for assignment,
/// where the declared type isn't carried but the current value's type is it).
pub(crate) fn coerce_like(new: Value, existing: &Value) -> Value {
    match existing {
        Value::UInt(_) => coerce(new, &Type::UInt),
        Value::Float64(_) => coerce(new, &Type::Float64),
        _ => new,
    }
}
