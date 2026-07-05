use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use crate::ast::{Dtype, Expr, FnParam, InfixOp, Method, Param, PrefixOp, Stmt, Type};
use crate::error::RuntimeError;

/// A lexical scope: its own bindings plus a link to the enclosing scope.
/// Scopes are shared (`Rc`) and mutable (`RefCell`) so a closure can capture
/// the exact scope it was defined in.
struct Scope {
    vars: HashMap<String, Value>,
    parent: Option<Env>,
}

type Env = Rc<RefCell<Scope>>;

impl Scope {
    fn global() -> Env {
        Rc::new(RefCell::new(Scope {
            vars: HashMap::new(),
            parent: None,
        }))
    }

    /// A new empty scope whose parent is `parent` (used for call frames).
    fn child(parent: &Env) -> Env {
        Rc::new(RefCell::new(Scope {
            vars: HashMap::new(),
            parent: Some(parent.clone()),
        }))
    }
}

/// Bind `name` in the current (innermost) scope. Re-binding shadows any
/// binding inherited from an enclosing scope.
fn define(env: &Env, name: &str, value: Value) {
    env.borrow_mut().vars.insert(name.to_string(), value);
}

/// Re-assign an existing binding: walk outward to the nearest scope that already
/// defines `name` and overwrite it there (so assigning from inside a block updates
/// the enclosing variable rather than shadowing it). Errors if `name` is unbound.
fn assign(env: &Env, name: &str, value: Value) -> Result<(), RuntimeError> {
    let mut current = env.clone();
    loop {
        {
            let mut scope = current.borrow_mut();
            if let Some(existing) = scope.vars.get(name) {
                // Keep the variable's numeric type stable across assignment.
                let coerced = coerce_like(value, existing);
                scope.vars.insert(name.to_string(), coerced);
                return Ok(());
            }
        }
        let parent = current.borrow().parent.clone();
        match parent {
            Some(p) => current = p,
            // No enclosing scope defines `name`: this is a `var`-less variable
            // introduction (`x = e` on an undeclared name). Mojo allows it, but
            // mojo-lite requires an explicit `var` — parsed and checked, flagged
            // unsupported here (the "parse now, run later" strategy).
            None => {
                return Err(RuntimeError::Unsupported(format!(
                    "introducing a variable without 'var' ('{name} = …'); write 'var {name}: T = …'"
                )));
            }
        }
    }
}

/// Look up `name`, walking outward through the scope chain (lexical lookup).
fn lookup(env: &Env, name: &str) -> Option<Value> {
    let scope = env.borrow();
    if let Some(value) = scope.vars.get(name) {
        return Some(value.clone());
    }
    let parent = scope.parent.clone();
    drop(scope);
    match parent {
        Some(p) => lookup(&p, name),
        None => None,
    }
}

/// A function value: its parameters, body, and the scope it closed over.
pub struct Closure {
    pub params: Vec<FnParam>,
    /// Compile-time parameters, `(name, is_value)`, in declaration order — matched
    /// positionally against explicit `[...]` arguments at a call. Type parameters
    /// are erased; value parameters are bound as `Int` locals in the call frame.
    param_decls: Vec<(String, bool)>,
    pub body: Vec<Stmt>,
    /// The scope in which the `def` was evaluated — captured for lexical scoping.
    env: Env,
    /// `mut self` — a method that mutates its receiver; the (possibly changed)
    /// `self` is written back to the receiver variable after the call. Always
    /// `false` for a plain `def`/closure.
    mut_self: bool,
}

// Custom Debug that deliberately does NOT recurse into the captured `env`
// (which can point back at this very closure, creating a cycle).
impl fmt::Debug for Closure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Closure")
            .field("params", &self.params)
            .field("arity", &self.params.len())
            .finish_non_exhaustive()
    }
}

/// A runtime value produced by evaluating an expression.
#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    UInt(u64),
    Float64(f64),
    Bool(bool),
    Str(String),
    None,
    Closure(Rc<Closure>),
    /// A half-open integer range `[start, stop)` with the given `step`, produced
    /// by the built-in `range(...)` and consumed by `for`. Not a first-class
    /// value: there is no annotation for it, so it only lives in a `for` header.
    Range { start: i64, stop: i64, step: i64 },
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
    Simd { dtype: Dtype, lanes: SimdLanes },
    /// An `Error` value carrying its message (what `raise` raises).
    Error(String),
    /// A `List` value — a value type (`Clone` deep-copies its elements, so
    /// assigning/passing a list copies it, matching Mojo's value semantics).
    List(Vec<Value>),
    /// A `Tuple` value — a fixed-size, heterogeneous value type (`Clone`
    /// deep-copies; immutable — no element write).
    Tuple(Vec<Value>),
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

/// A struct definition: field order (for the fieldwise constructor) and methods.
/// Methods are `Closure`s capturing the scope the `struct` was defined in; a call
/// binds `self` (the receiver) plus the declared parameters.
pub struct StructDef {
    /// Fields in declaration order: `(name, type)` — the type drives literal
    /// coercion of constructor arguments.
    fields: Vec<(String, Type)>,
    /// Compile-time parameters, `(name, is_value)`, in declaration order — matched
    /// positionally against explicit `[...]` arguments; value ones are reified.
    param_decls: Vec<(String, bool)>,
    methods: HashMap<String, Rc<Closure>>,
    fieldwise_init: bool,
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
            (Value::Closure(a), Value::Closure(b)) => Rc::ptr_eq(a, b),
            (
                Value::Range { start: s1, stop: e1, step: t1 },
                Value::Range { start: s2, stop: e2, step: t2 },
            ) => s1 == s2 && e1 == e2 && t1 == t2,
            (
                Value::Struct { name: n1, fields: f1, value_params: p1 },
                Value::Struct { name: n2, fields: f2, value_params: p2 },
            ) => n1 == n2 && f1 == f2 && p1 == p2,
            (
                Value::Simd { dtype: d1, lanes: l1 },
                Value::Simd { dtype: d2, lanes: l2 },
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
            Value::Closure(c) => write!(f, "<closure/{}>", c.params.len()),
            Value::Range { start, stop, step } => write!(f, "range({}, {}, {})", start, stop, step),
            Value::Struct { name, fields, value_params } => {
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
fn classify_param_decls(type_params: &[crate::ast::TypeParam]) -> Vec<(String, bool)> {
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
fn type_name(value: &Value) -> String {
    match value {
        Value::Int(_) => "Int".to_string(),
        Value::UInt(_) => "UInt".to_string(),
        Value::Float64(_) => "Float64".to_string(),
        Value::Bool(_) => "Bool".to_string(),
        Value::Str(_) => "String".to_string(),
        Value::None => "None".to_string(),
        Value::Closure(_) => "closure".to_string(),
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
        Value::List(_) => "List".to_string(),
        Value::Tuple(items) => {
            let elems: Vec<String> = items.iter().map(type_name).collect();
            format!("Tuple[{}]", elems.join(", "))
        }
    }
}

/// Structural equality for `==`/`!=`. Defined only between values of the same
/// scalar type; comparing across types (or closures) is a type error.
fn values_equal(a: &Value, b: &Value) -> Result<bool, RuntimeError> {
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
/// core of `eval_infix` and augmented assignment. `and`/`or` short-circuit and so
/// are handled by the caller, not here.
fn apply_infix(op: InfixOp, l: Value, r: Value) -> Result<Value, RuntimeError> {
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
    u32::try_from(y)
        .map_err(|_| RuntimeError::TypeError("'**' exponent must be a non-negative Int that fits in 32 bits".to_string()))
}

/// Python/Mojo floor division: round toward negative infinity.
fn floor_div(x: i64, y: i64) -> i64 {
    let q = x / y;
    let r = x % y;
    if r != 0 && ((r < 0) != (y < 0)) { q - 1 } else { q }
}

/// Python/Mojo modulo: the result takes the sign of the divisor.
fn floor_mod(x: i64, y: i64) -> i64 {
    let r = x % y;
    if r != 0 && ((r < 0) != (y < 0)) { r + y } else { r }
}

// --- Places (assignment targets) ---

/// One step in a place path: a struct field or a list index.
enum PathStep {
    Field(String),
    Index(i64),
}

/// Navigate a mutable value along a place path, returning a mutable reference to
/// the innermost slot (which the caller overwrites). Descends into struct fields
/// and list elements in place.
fn navigate<'a>(mut slot: &'a mut Value, path: &[PathStep]) -> Result<&'a mut Value, RuntimeError> {
    for step in path {
        slot = match step {
            PathStep::Field(name) => match slot {
                Value::Struct { fields, .. } => fields
                    .iter_mut()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v)
                    .ok_or_else(|| RuntimeError::TypeError(format!("no field '{}'", name)))?,
                other => {
                    return Err(RuntimeError::TypeError(format!(
                        "cannot access field '{}' of {}",
                        name,
                        type_name(other)
                    )));
                }
            },
            PathStep::Index(i) => match slot {
                Value::List(items) => {
                    let idx = bounds_check(*i, items.len(), "list")?;
                    &mut items[idx]
                }
                other => {
                    return Err(RuntimeError::TypeError(format!("cannot index {}", type_name(other))));
                }
            },
        };
    }
    Ok(slot)
}

/// Run `f` on the mutable slot at place `root`+`path`, walking outward to the
/// scope that defines `root`. The single entry point for reading or mutating a
/// place in the variable's binding (so value semantics hold: only that binding
/// changes).
fn at_place<T>(
    env: &Env,
    root: &str,
    path: &[PathStep],
    f: impl FnOnce(&mut Value) -> Result<T, RuntimeError>,
) -> Result<T, RuntimeError> {
    let mut current = env.clone();
    loop {
        {
            let mut scope = current.borrow_mut();
            if let Some(root_slot) = scope.vars.get_mut(root) {
                let slot = navigate(root_slot, path)?;
                return f(slot);
            }
        }
        let parent = current.borrow().parent.clone();
        match parent {
            Some(p) => current = p,
            None => return Err(RuntimeError::UndefinedVariable(root.to_string())),
        }
    }
}

/// Read-modify-write the target of a place: navigate to the container of the
/// final step, then apply `compute(current) -> new`. Handles a **SIMD lane** as
/// the final step specially (a lane is not a `Value` slot), and an ordinary
/// variable/field/list-element slot otherwise. Shared by plain assignment
/// (`compute` ignores the current value) and augmented assignment.
fn update_place(
    env: &Env,
    root: &str,
    path: &[PathStep],
    compute: impl FnOnce(Value) -> Result<Value, RuntimeError>,
) -> Result<(), RuntimeError> {
    match path.split_last() {
        // The place is the root variable itself (an empty path).
        None => at_place(env, root, &[], |slot| {
            let new = compute(slot.clone())?;
            *slot = coerce_like(new, slot);
            Ok(())
        }),
        Some((last, prefix)) => at_place(env, root, prefix, |container| {
            // A SIMD lane target: `container[i]` where `container` is a SIMD.
            if let PathStep::Index(i) = last
                && let Value::Simd { dtype, lanes } = container
            {
                let idx = *i;
                let current = read_simd_lane(*dtype, lanes, idx)?;
                let new = compute(current)?;
                return set_simd_lane(*dtype, lanes, idx, new);
            }
            // An ordinary `Value` slot (struct field or list element).
            let slot = navigate(container, std::slice::from_ref(last))?;
            let new = compute(slot.clone())?;
            *slot = coerce_like(new, slot);
            Ok(())
        }),
    }
}

/// Assign `value` into the place (a plain write ignores the current value).
fn set_place(env: &Env, root: &str, path: &[PathStep], value: Value) -> Result<(), RuntimeError> {
    update_place(env, root, path, move |_current| Ok(value))
}

/// Read (clone) the current value at a place.
fn read_place(env: &Env, root: &str, path: &[PathStep]) -> Result<Value, RuntimeError> {
    at_place(env, root, path, |slot| Ok(slot.clone()))
}

// --- Collections / indexing ---

/// Convert a signed index into a bounds-checked `usize`, or a runtime error.
fn bounds_check(idx: i64, len: usize, what: &str) -> Result<usize, RuntimeError> {
    if idx < 0 || idx as usize >= len {
        return Err(RuntimeError::TypeError(format!(
            "{} index {} out of range 0..{}",
            what, idx, len
        )));
    }
    Ok(idx as usize)
}

/// A `Value` used as an index, requiring it to be an `Int`.
fn value_as_index(v: &Value) -> Result<i64, RuntimeError> {
    match v {
        Value::Int(n) => Ok(*n),
        other => Err(RuntimeError::TypeError(format!(
            "index must be Int, got {}",
            type_name(other)
        ))),
    }
}

/// Whether a `List` method mutates the list (vs. the read-only queries).
fn is_list_mutator(method: &str) -> bool {
    matches!(
        method,
        "append" | "insert" | "remove" | "pop" | "clear" | "reverse" | "extend"
    )
}

/// Apply a `List` method to an owned (mutable) list. Mutating methods change
/// `items`; the query methods (`count`/`index`) delegate to [`list_query`]. An
/// incoming element is coerced to the existing elements' numeric kind.
fn apply_list_method(items: &mut Vec<Value>, method: &str, args: &[Value]) -> Result<Value, RuntimeError> {
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
            match items.iter().position(|it| values_equal(it, &target).unwrap_or(false)) {
                Some(p) => {
                    items.remove(p);
                    Ok(Value::None)
                }
                None => Err(RuntimeError::TypeError("remove(x): x is not in the list".to_string())),
            }
        }
        "pop" => {
            let i = if let Some(a) = args.first() {
                bounds_check(value_as_index(a)?, items.len(), "list")?
            } else if items.is_empty() {
                return Err(RuntimeError::TypeError("pop() from an empty list".to_string()));
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
        _ => Err(RuntimeError::TypeError(format!("List has no method '{}'", method))),
    }
}

/// A read-only `List` query: `count(x)` → the number of equal elements;
/// `index(x)` → the first equal element's index (a runtime error if absent).
fn list_query(items: &[Value], method: &str, args: &[Value]) -> Result<Value, RuntimeError> {
    let target = match items.first() {
        Some(f) => coerce_like(args[0].clone(), f),
        None => args[0].clone(),
    };
    let eq = |it: &Value| values_equal(it, &target).unwrap_or(false);
    match method {
        "count" => Ok(Value::Int(items.iter().filter(|it| eq(it)).count() as i64)),
        "index" => match items.iter().position(eq) {
            Some(p) => Ok(Value::Int(p as i64)),
            None => Err(RuntimeError::TypeError("index(x): x is not in the list".to_string())),
        },
        _ => Err(RuntimeError::TypeError(format!("List has no method '{}'", method))),
    }
}

/// Evaluate `x in c` / `x not in c` → `Bool`. `c` is a `List` (element
/// membership, comparing the coerced value) or a `String` (substring test).
fn eval_membership(op: InfixOp, l: &Value, r: &Value) -> Result<Value, RuntimeError> {
    let found = match r {
        Value::List(items) => {
            let target = match items.first() {
                Some(f) => coerce_like(l.clone(), f),
                None => l.clone(),
            };
            items.iter().any(|it| values_equal(it, &target).unwrap_or(false))
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
fn promote_numeric_elems(items: &mut [Value]) {
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
        Value::Simd { lanes: SimdLanes::Int(l), .. } if l.len() == 1 => Ok(l[0]),
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
        Value::Simd { lanes: SimdLanes::Float(l), .. } if l.len() == 1 => Ok(l[0]),
        other => Err(RuntimeError::TypeError(format!(
            "cannot use {} as a float SIMD element",
            type_name(other)
        ))),
    }
}

/// A scalar value's boolean content, for building a `bool` SIMD lane.
fn value_to_bool_lane(v: &Value) -> Result<bool, RuntimeError> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Simd { lanes: SimdLanes::Bool(l), .. } if l.len() == 1 => Ok(l[0]),
        other => Err(RuntimeError::TypeError(format!(
            "cannot use {} as a bool SIMD element",
            type_name(other)
        ))),
    }
}

/// Read lane `i` of a SIMD as a width-1 SIMD (scalar) of the same dtype — the
/// shared core of `v[i]` reads and SIMD-lane read-modify-write.
fn read_simd_lane(dtype: Dtype, lanes: &SimdLanes, i: i64) -> Result<Value, RuntimeError> {
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
fn set_simd_lane(dtype: Dtype, lanes: &mut SimdLanes, i: i64, value: Value) -> Result<(), RuntimeError> {
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
fn simd_binop(op: InfixOp, l: &Value, r: &Value) -> Result<Value, RuntimeError> {
    use InfixOp::*;
    let (dtype, width) = simd_shape(l).or_else(|| simd_shape(r)).expect("a SIMD operand");
    let is_cmp = matches!(op, Eq | Ne | Lt | Gt | Le | Ge);
    match dtype {
        Dtype::Bool => {
            let xs = to_bool_lanes(l, width)?;
            let ys = to_bool_lanes(r, width)?;
            let out: Vec<bool> = xs.iter().zip(&ys).map(|(a, b)| match op {
                Eq => a == b,
                Ne => a != b,
                _ => false, // checker rejects other ops on bool lanes
            }).collect();
            Ok(Value::Simd { dtype: Dtype::Bool, lanes: SimdLanes::Bool(out) })
        }
        d if d.is_float() => {
            let xs = to_float_lanes(l, dtype, width)?;
            let ys = to_float_lanes(r, dtype, width)?;
            if is_cmp {
                let out = xs.iter().zip(&ys).map(|(a, b)| float_cmp(op, *a, *b)).collect();
                Ok(Value::Simd { dtype: Dtype::Bool, lanes: SimdLanes::Bool(out) })
            } else {
                let out: Vec<f64> = xs.iter().zip(&ys).map(|(a, b)| round_lane(dtype, float_arith(op, *a, *b))).collect();
                Ok(Value::Simd { dtype, lanes: SimdLanes::Float(out) })
            }
        }
        d => {
            let xs = to_int_lanes(l, d, width)?;
            let ys = to_int_lanes(r, d, width)?;
            if is_cmp {
                let out = xs.iter().zip(&ys).map(|(a, b)| int_cmp(op, *a, *b)).collect();
                Ok(Value::Simd { dtype: Dtype::Bool, lanes: SimdLanes::Bool(out) })
            } else {
                let out: Vec<i128> = xs.iter().zip(&ys).map(|(a, b)| wrap(d, int_arith(op, *a, *b))).collect();
                Ok(Value::Simd { dtype: d, lanes: SimdLanes::Int(out) })
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
        Value::Simd { lanes: SimdLanes::Int(l), .. } => Ok(l.clone()),
        scalar => Ok(vec![wrap(dtype, value_to_int(scalar)?); width]),
    }
}

fn to_float_lanes(v: &Value, dtype: Dtype, width: usize) -> Result<Vec<f64>, RuntimeError> {
    match v {
        Value::Simd { lanes: SimdLanes::Float(l), .. } => Ok(l.clone()),
        scalar => Ok(vec![round_lane(dtype, value_to_float(scalar)?); width]),
    }
}

fn to_bool_lanes(v: &Value, width: usize) -> Result<Vec<bool>, RuntimeError> {
    match v {
        Value::Simd { lanes: SimdLanes::Bool(l), .. } => Ok(l.clone()),
        scalar => Ok(vec![value_to_bool_lane(scalar)?; width]),
    }
}

/// Coerce a value to a declared type at a binding site. Only materializes a
/// numeric literal's default (`Int`/`Float64`) into another numeric type; all
/// other values pass through unchanged.
fn coerce(value: Value, ty: &Type) -> Value {
    use crate::ast::{Expr, ParamArg};
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
                        Some(ParamArg::Value(Expr::Identifier(id))) => {
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
fn coerce_like(new: Value, existing: &Value) -> Value {
    match existing {
        Value::UInt(_) => coerce(new, &Type::UInt),
        Value::Float64(_) => coerce(new, &Type::Float64),
        _ => new,
    }
}

/// Control-flow signal threaded through statement execution so `return` can
/// unwind to the enclosing function call.
enum Flow {
    Normal,
    Return(Value),
    Break,
    Continue,
}

/// Tree-walking interpreter for the parsed AST.
///
/// Scoping is lexical with shadowing; evaluation is strict and call-by-value.
/// Closures capture their defining scope but may not escape it (downward
/// funargs only — see `RuntimeError::ClosureEscape`).
pub struct Evaluator {
    global: Env,
    /// Defined structs, by name. Filled as `struct` statements execute; read for
    /// construction and method dispatch. `RefCell` because evaluation is `&self`.
    structs: RefCell<HashMap<String, Rc<StructDef>>>,
    /// Captured `print` output (`RefCell` because evaluation is `&self`). Kept in
    /// a buffer rather than written to stdout so it is testable; the caller
    /// decides what to do with it (see [`Evaluator::output`]).
    output: RefCell<String>,
}

impl Evaluator {
    pub fn new() -> Self {
        Self {
            global: Scope::global(),
            structs: RefCell::new(HashMap::new()),
            output: RefCell::new(String::new()),
        }
    }

    /// The text written by `print` so far.
    pub fn output(&self) -> String {
        self.output.borrow().clone()
    }

    /// Evaluate a whole program in the global scope.
    pub fn eval_program(&mut self, stmts: &[Stmt]) -> Result<(), RuntimeError> {
        let env = self.global.clone();
        self.exec_block(stmts, &env)?;
        // Entry point: after the top-level statements, if a zero-argument `main`
        // was defined, call it — mirroring Mojo, where `main` is the entry point.
        if let Some(Value::Closure(c)) = lookup(&env, "main")
            && c.params.is_empty()
        {
            let call = Expr::Call {
                name: "main".to_string(),
                param_args: Vec::new(),
                args: Vec::new(),
                kwargs: Vec::new(),
            };
            self.eval_expr(&call, &env)?;
        }
        Ok(())
    }

    /// Execute statements in `env`, stopping early on any non-local control
    /// flow (`return`/`break`/`continue`), which propagates to the caller.
    fn exec_block(&self, stmts: &[Stmt], env: &Env) -> Result<Flow, RuntimeError> {
        for stmt in stmts {
            let flow = self.exec_stmt(stmt, env)?;
            if !matches!(flow, Flow::Normal) {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_stmt(&self, stmt: &Stmt, env: &Env) -> Result<Flow, RuntimeError> {
        match stmt {
            // Type annotations are parsed but not yet enforced (no static
            // type checking pass yet) — the evaluator is dynamically typed.
            Stmt::VarDecl { name, ty, value } => {
                let value = self.eval_expr(value, env)?;
                // Annotated: coerce a literal to the declared type. Inferred
                // (`var x = e`): keep the value's natural type (an int literal is
                // already `Int`, a float literal `Float64`, etc.).
                let value = match ty {
                    Some(t) => coerce(value, t),
                    None => value,
                };
                define(env, name, value);
                Ok(Flow::Normal)
            }
            Stmt::Assign { name, value } => {
                let value = self.eval_expr(value, env)?;
                // Mirror the return rule: a closure may not be moved by assignment.
                if matches!(value, Value::Closure(_)) {
                    return Err(RuntimeError::ClosureEscape);
                }
                assign(env, name, value)?;
                Ok(Flow::Normal)
            }
            // Tuple unpacking is flagged by the checker; this arm is a safety net.
            Stmt::Unpack { .. } => {
                Err(RuntimeError::Unsupported("tuple unpacking".to_string()))
            }
            // `with` is flagged by the checker; this arm is a safety net.
            Stmt::With { .. } => {
                Err(RuntimeError::Unsupported("with statement".to_string()))
            }
            Stmt::SetPlace { place, value } => {
                // Decompose the place into a root variable + a navigation path,
                // evaluating any index expressions in the current scope first.
                let (root, path) = self.decompose_place(place, env)?;
                let value = self.eval_expr(value, env)?;
                set_place(env, &root, &path, value)?;
                Ok(Flow::Normal)
            }
            Stmt::AugAssign { place, op, value } => {
                // Read-modify-write in one place traversal: decompose the place
                // once (so an index like `xs[f()]` is evaluated a single time),
                // then apply `target = target OP value` in place (also on a lane).
                let (root, path) = self.decompose_place(place, env)?;
                let rhs = self.eval_expr(value, env)?;
                update_place(env, &root, &path, move |current| apply_infix(*op, current, rhs))?;
                Ok(Flow::Normal)
            }
            Stmt::Def { name, type_params, params, body, .. } => {
                let closure = Value::Closure(Rc::new(Closure {
                    params: params.clone(),
                    param_decls: classify_param_decls(type_params),
                    body: body.clone(),
                    env: env.clone(), // capture the defining scope (lexical)
                    mut_self: false,
                }));
                define(env, name, closure);
                Ok(Flow::Normal)
            }
            // Type parameters and trait conformance are erased at runtime: a
            // generic struct runs as ordinary dynamic code, so `type_params` and
            // `conforms` are ignored here.
            Stmt::Struct { name, type_params, fields, methods, fieldwise_init, conforms: _, decorators: _ } => {
                self.register_struct(name, type_params, fields, methods, *fieldwise_init, env);
                Ok(Flow::Normal)
            }
            // Traits are a pure compile-time construct: method dispatch is on the
            // conforming struct (which already carries the method), so a `trait`
            // needs no runtime representation.
            Stmt::Trait { .. } => Ok(Flow::Normal),
            // A compile-time constant is also an ordinary `Int` binding at runtime.
            Stmt::Comptime { name, value } => {
                let value = self.eval_expr(value, env)?;
                define(env, name, value);
                Ok(Flow::Normal)
            }
            // `comptime if` / `comptime for` are flagged by the checker; these arms
            // are safety nets.
            Stmt::ComptimeIf { .. } => {
                Err(RuntimeError::Unsupported("comptime if".to_string()))
            }
            Stmt::ComptimeFor { .. } => {
                Err(RuntimeError::Unsupported("comptime for".to_string()))
            }
            Stmt::If { branches, orelse } => {
                for (cond, body) in branches {
                    if self.eval_bool(cond, env)? {
                        // Each branch body runs in its own nested scope.
                        return self.exec_block(body, &Scope::child(env));
                    }
                }
                match orelse {
                    Some(body) => self.exec_block(body, &Scope::child(env)),
                    None => Ok(Flow::Normal),
                }
            }
            Stmt::While { cond, body } => {
                while self.eval_bool(cond, env)? {
                    match self.exec_block(body, &Scope::child(env))? {
                        Flow::Break => break,
                        Flow::Normal | Flow::Continue => {}
                        ret @ Flow::Return(_) => return Ok(ret),
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::For { var, iter, body } => {
                // Materialize the per-iteration values: `Int`s for a range, or the
                // elements of a `List`.
                let items: Vec<Value> = match self.eval_expr(iter, env)? {
                    Value::Range { start, stop, step } => {
                        let mut v = Vec::new();
                        let mut i = start;
                        while (step > 0 && i < stop) || (step < 0 && i > stop) {
                            v.push(Value::Int(i));
                            i += step;
                        }
                        v
                    }
                    Value::List(items) => items,
                    other => {
                        return Err(RuntimeError::TypeError(format!(
                            "'for' loop requires a range or List, got {}",
                            type_name(&other)
                        )));
                    }
                };
                for item in items {
                    // Fresh scope per iteration; the loop variable is bound in it.
                    let scope = Scope::child(env);
                    define(&scope, var, item);
                    match self.exec_block(body, &scope)? {
                        Flow::Break => break,
                        Flow::Normal | Flow::Continue => {}
                        ret @ Flow::Return(_) => return Ok(ret),
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::Return(expr) => {
                let value = match expr {
                    Some(expr) => self.eval_expr(expr, env)?,
                    None => Value::None,
                };
                // Downward funargs only: a closure may not leave its scope.
                if matches!(value, Value::Closure(_)) {
                    return Err(RuntimeError::ClosureEscape);
                }
                Ok(Flow::Return(value))
            }
            Stmt::Pass => Ok(Flow::Normal),
            Stmt::Break => Ok(Flow::Break),
            Stmt::Continue => Ok(Flow::Continue),
            // `raise` — evaluate the operand (a `String` is wrapped in an `Error`)
            // and propagate it as a `Raised` error, which `?` carries up through
            // the evaluator until a `try` intercepts it (or it reaches the top).
            Stmt::Raise(expr) => {
                let msg = match self.eval_expr(expr, env)? {
                    Value::Error(m) => m,
                    Value::Str(s) => s,
                    other => {
                        return Err(RuntimeError::TypeError(format!(
                            "cannot raise {}",
                            type_name(&other)
                        )));
                    }
                };
                Err(RuntimeError::Raised(msg))
            }
            // Imports are parsed but not resolved (no module system yet).
            Stmt::Import { .. } | Stmt::FromImport { .. } => Ok(Flow::Normal),
            Stmt::Try { body, except, orelse, finalbody } => {
                self.exec_try(body, except, orelse, finalbody, env)
            }
            Stmt::Expr(expr) => {
                self.eval_expr(expr, env)?;
                Ok(Flow::Normal)
            }
        }
    }

    /// Execute a `try`/`except`/`else`/`finally`. A `Raised` error from the body
    /// is caught by `except` (binding the error name if given); `else` runs only
    /// when the body completed normally; `finally` always runs and its own
    /// non-`Normal` outcome (a `return`/`break`, or an error) takes precedence.
    fn exec_try(
        &self,
        body: &[Stmt],
        except: &Option<(Option<String>, Vec<Stmt>)>,
        orelse: &Option<Vec<Stmt>>,
        finalbody: &Option<Vec<Stmt>>,
        env: &Env,
    ) -> Result<Flow, RuntimeError> {
        let try_result = self.exec_block(body, &Scope::child(env));
        let outcome = match try_result {
            // The body raised: run `except` (if any), else re-propagate.
            Err(RuntimeError::Raised(msg)) => match except {
                Some((name, ex_body)) => {
                    let scope = Scope::child(env);
                    if let Some(n) = name {
                        define(&scope, n, Value::Error(msg));
                    }
                    self.exec_block(ex_body, &scope)
                }
                None => Err(RuntimeError::Raised(msg)),
            },
            // A real (non-raised) runtime error propagates.
            Err(other) => Err(other),
            // The body completed normally: run `else` (if any).
            Ok(Flow::Normal) => match orelse {
                Some(else_body) => self.exec_block(else_body, &Scope::child(env)),
                None => Ok(Flow::Normal),
            },
            // A `return`/`break`/`continue` from the body propagates.
            Ok(flow) => Ok(flow),
        };
        // `finally` always runs; a non-`Normal` outcome from it wins.
        if let Some(fin) = finalbody {
            match self.exec_block(fin, &Scope::child(env))? {
                Flow::Normal => outcome,
                other => Ok(other),
            }
        } else {
            outcome
        }
    }

    fn eval_expr(&self, expr: &Expr, env: &Env) -> Result<Value, RuntimeError> {
        match expr {
            Expr::Int(n) => Ok(Value::Int(*n)),
            Expr::Float(x) => Ok(Value::Float64(*x)),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Str(s) => Ok(Value::Str(s.clone())),
            Expr::None => Ok(Value::None),
            Expr::Identifier(name) => {
                lookup(env, name).ok_or_else(|| RuntimeError::UndefinedVariable(name.clone()))
            }
            Expr::Prefix(op, operand) => self.eval_prefix(*op, operand, env),
            Expr::Infix(op, left, right) => self.eval_infix(*op, left, right, env),
            Expr::Call { name, param_args, args, kwargs } => {
                self.eval_call(name, param_args, args, kwargs, env)
            }
            Expr::Member { object, field } => self.eval_member(object, field, env),
            Expr::MethodCall { object, method, args, kwargs: _ } => {
                self.eval_method_call(object, method, args, env)
            }
            Expr::Index { object, index } => self.eval_index(object, index, env),
            // Transfer (ownership move) is not modeled: evaluate the operand.
            Expr::Transfer(inner) => self.eval_expr(inner, env),
            Expr::ListLit(elems) => Ok(Value::List(self.eval_list_elems(elems, env)?)),
            Expr::TupleLit(elems) => {
                let mut items = Vec::with_capacity(elems.len());
                for e in elems {
                    items.push(self.eval_expr(e, env)?);
                }
                Ok(Value::Tuple(items))
            }
            // The walrus operator is parsed and type-checked but not run.
            Expr::Named { name, .. } => Err(RuntimeError::Unsupported(format!(
                "the walrus operator ':=' ('{name} := …'); assign on its own line with 'var'"
            ))),
            // Parsed, semantics deferred (syntax-first phase). The checker flags
            // these first, so these arms are a safety net.
            Expr::IfExpr { .. } => {
                Err(RuntimeError::Unsupported("conditional expression".to_string()))
            }
            Expr::Compare { .. } => {
                Err(RuntimeError::Unsupported("chained comparison".to_string()))
            }
            Expr::Slice { .. } => {
                Err(RuntimeError::Unsupported("slice subscript".to_string()))
            }
            Expr::TString { .. } => Err(RuntimeError::Unsupported("t-string".to_string())),
        }
    }

    /// Evaluate list elements and promote numeric elements to a common kind (so
    /// `[1, 2.0]` becomes `[1.0, 2.0]`, matching the checker's element type).
    fn eval_list_elems(&self, elems: &[Expr], env: &Env) -> Result<Vec<Value>, RuntimeError> {
        let mut items = Vec::with_capacity(elems.len());
        for e in elems {
            items.push(self.eval_expr(e, env)?);
        }
        promote_numeric_elems(&mut items);
        Ok(items)
    }

    /// Decompose a place expression into its root variable name and the path of
    /// field/index steps to the target slot, evaluating index expressions in the
    /// current scope. (`p.items[i].x` → `("p", [Field(items), Index(i), Field(x)])`.)
    fn decompose_place(&self, place: &Expr, env: &Env) -> Result<(String, Vec<PathStep>), RuntimeError> {
        match place {
            Expr::Identifier(name) => Ok((name.clone(), Vec::new())),
            Expr::Member { object, field } => {
                let (root, mut path) = self.decompose_place(object, env)?;
                path.push(PathStep::Field(field.clone()));
                Ok((root, path))
            }
            Expr::Index { object, index } => {
                let (root, mut path) = self.decompose_place(object, env)?;
                let i = value_as_index(&self.eval_expr(index, env)?)?;
                path.push(PathStep::Index(i));
                Ok((root, path))
            }
            _ => Err(RuntimeError::TypeError("invalid assignment target".to_string())),
        }
    }

    /// A subscript `object[index]`: read a SIMD lane, returning it as a width-1
    /// SIMD (scalar) of the same dtype — or the element of a `List`.
    fn eval_index(&self, object: &Expr, index: &Expr, env: &Env) -> Result<Value, RuntimeError> {
        let obj = self.eval_expr(object, env)?;
        let idx = match self.eval_expr(index, env)? {
            Value::Int(i) => i,
            other => {
                return Err(RuntimeError::TypeError(format!(
                    "index must be Int, got {}",
                    type_name(&other)
                )));
            }
        };
        match obj {
            Value::Simd { dtype, lanes } => read_simd_lane(dtype, &lanes, idx),
            Value::List(items) => {
                let i = bounds_check(idx, items.len(), "list")?;
                Ok(items[i].clone())
            }
            Value::Tuple(items) => {
                // The checker fixes the index at compile time; guard anyway.
                let i = bounds_check(idx, items.len(), "tuple")?;
                Ok(items[i].clone())
            }
            other => Err(RuntimeError::TypeError(format!("cannot index {}", type_name(&other)))),
        }
    }

    /// Field access `object.field`: read a field off a struct value. `Self.n`
    /// reads the enclosing struct's reified value parameter off `self`.
    fn eval_member(&self, object: &Expr, field: &str, env: &Env) -> Result<Value, RuntimeError> {
        if let Expr::Identifier(s) = object
            && s == "Self"
            && let Some(Value::Struct { value_params, .. }) = lookup(env, "self")
            && let Some((_, v)) = value_params.iter().find(|(n, _)| n == field)
        {
            return Ok(v.clone());
        }
        match self.eval_expr(object, env)? {
            Value::Struct { name, fields, .. } => fields
                .iter()
                .find(|(n, _)| n == field)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| {
                    RuntimeError::TypeError(format!("'{}' has no field '{}'", name, field))
                }),
            other => Err(RuntimeError::TypeError(format!(
                "cannot access field '{}' of {}",
                field,
                type_name(&other)
            ))),
        }
    }

    /// Method call `object.method(args)`. When the receiver is a writable **place**
    /// (a variable, or a field/index chain rooted at one), a mutating method (a
    /// `List` mutator, or a `mut self` struct method) mutates that place's binding
    /// in place; otherwise the receiver is an ordinary value (only read-only calls
    /// reach here — the checker rejects mutation of a temporary).
    fn eval_method_call(
        &self,
        object: &Expr,
        method: &str,
        args: &[Expr],
        env: &Env,
    ) -> Result<Value, RuntimeError> {
        // Evaluate arguments in the caller's scope first.
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            arg_vals.push(self.eval_expr(a, env)?);
        }
        // A place receiver (variable / field / index chain) — resolved once so
        // index expressions are evaluated a single time.
        if let Ok((root, path)) = self.decompose_place(object, env) {
            let recv = read_place(env, &root, &path)?;
            match recv {
                Value::List(items) if !is_list_mutator(method) => {
                    return list_query(&items, method, &arg_vals);
                }
                Value::List(_) => {
                    return at_place(env, &root, &path, |slot| match slot {
                        Value::List(items) => apply_list_method(items, method, &arg_vals),
                        other => Err(RuntimeError::TypeError(format!(
                            "cannot call '{}' on {}",
                            method,
                            type_name(other)
                        ))),
                    });
                }
                Value::Struct { ref name, .. } => {
                    let name = name.clone();
                    let (ret, new_self, mut_self) =
                        self.run_struct_method(&name, method, recv, arg_vals)?;
                    if mut_self {
                        set_place(env, &root, &path, new_self)?;
                    }
                    return Ok(ret);
                }
                other => {
                    return Err(RuntimeError::TypeError(format!(
                        "cannot call method '{}' on {}",
                        method,
                        type_name(&other)
                    )));
                }
            }
        }
        // A non-place receiver (a temporary / call result): evaluate it. Only
        // read-only calls reach here (the checker rejects mutating a temporary).
        let receiver = self.eval_expr(object, env)?;
        match receiver {
            Value::List(items) => list_query(&items, method, &arg_vals),
            Value::Struct { ref name, .. } => {
                let name = name.clone();
                let (ret, _self, _mut) = self.run_struct_method(&name, method, receiver, arg_vals)?;
                Ok(ret)
            }
            other => Err(RuntimeError::TypeError(format!(
                "cannot call method '{}' on {}",
                method,
                type_name(&other)
            ))),
        }
    }

    /// Run struct `name`'s method `method` with `receiver` bound to `self` and
    /// `arg_values` to the params. Returns `(return value, the final self, whether
    /// the method is `mut self`)` — the caller writes `self` back for `mut self`.
    fn run_struct_method(
        &self,
        name: &str,
        method: &str,
        receiver: Value,
        arg_values: Vec<Value>,
    ) -> Result<(Value, Value, bool), RuntimeError> {
        let closure = self
            .structs
            .borrow()
            .get(name)
            .and_then(|def| def.methods.get(method).cloned())
            .ok_or_else(|| {
                RuntimeError::TypeError(format!("'{}' has no method '{}'", name, method))
            })?;
        if closure.params.len() != arg_values.len() {
            return Err(RuntimeError::ArityMismatch {
                name: method.to_string(),
                expected: closure.params.len(),
                got: arg_values.len(),
            });
        }
        // Call frame parent is the method's captured scope; bind `self` + params.
        let call_env = Scope::child(&closure.env);
        define(&call_env, "self", receiver);
        for (param, value) in closure.params.iter().zip(arg_values) {
            define(&call_env, &param.name, coerce(value, &param.ty));
        }
        let flow = self.exec_block(&closure.body, &call_env)?;
        let new_self = lookup(&call_env, "self").expect("self is bound in the call frame");
        let ret = match flow {
            Flow::Return(value) => value,
            _ => Value::None,
        };
        Ok((ret, new_self, closure.mut_self))
    }


    fn eval_prefix(&self, op: PrefixOp, operand: &Expr, env: &Env) -> Result<Value, RuntimeError> {
        let value = self.eval_expr(operand, env)?;
        match (op, value) {
            (PrefixOp::Neg, Value::Int(n)) => Ok(Value::Int(-n)),
            (PrefixOp::Neg, Value::Float64(x)) => Ok(Value::Float64(-x)),
            (PrefixOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
            (PrefixOp::Neg, v) => Err(RuntimeError::TypeError(format!("cannot negate {}", type_name(&v)))),
            (PrefixOp::Not, v) => Err(RuntimeError::TypeError(format!("'not' requires Bool, got {}", type_name(&v)))),
        }
    }

    fn eval_infix(&self, op: InfixOp, left: &Expr, right: &Expr, env: &Env) -> Result<Value, RuntimeError> {
        // `and` / `or` short-circuit, so the right operand is evaluated lazily.
        match op {
            InfixOp::And => {
                let l = self.eval_bool(left, env)?;
                if !l {
                    return Ok(Value::Bool(false));
                }
                return Ok(Value::Bool(self.eval_bool(right, env)?));
            }
            InfixOp::Or => {
                let l = self.eval_bool(left, env)?;
                if l {
                    return Ok(Value::Bool(true));
                }
                return Ok(Value::Bool(self.eval_bool(right, env)?));
            }
            _ => {}
        }

        let l = self.eval_expr(left, env)?;
        let r = self.eval_expr(right, env)?;
        apply_infix(op, l, r)
    }

    /// Evaluates an expression and requires it to be a `Bool`.
    fn eval_bool(&self, expr: &Expr, env: &Env) -> Result<bool, RuntimeError> {
        match self.eval_expr(expr, env)? {
            Value::Bool(b) => Ok(b),
            v => Err(RuntimeError::TypeError(format!(
                "expected Bool, got {}",
                type_name(&v)
            ))),
        }
    }

    fn eval_call(
        &self,
        name: &str,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
        env: &Env,
    ) -> Result<Value, RuntimeError> {
        let callee = match lookup(env, name) {
            Some(value) => value,
            // Built-ins and struct construction, resolved only when not shadowed.
            None => match name {
                "print" => return self.eval_print(args, env),
                "String" => return Ok(Value::Str(self.eval_expr(&args[0], env)?.to_string())),
                "abs" => return self.eval_abs(args, env),
                "min" | "max" => return self.eval_min_max(name == "min", args, env),
                "round" => {
                    let x = value_to_float(&self.eval_expr(&args[0], env)?)?;
                    return Ok(Value::Float64(x.round()));
                }
                "len" => {
                    return match self.eval_expr(&args[0], env)? {
                        Value::Str(s) => Ok(Value::Int(s.len() as i64)),
                        Value::List(items) => Ok(Value::Int(items.len() as i64)),
                        other => Err(RuntimeError::TypeError(format!(
                            "len() expects a String or List, got {}",
                            type_name(&other)
                        ))),
                    };
                }
                "List" => return self.eval_list_construction(param_args, args, env),
                "range" => return self.eval_range(args, env),
                "Int" | "UInt" | "Float64" => return self.eval_conversion(name, args, env),
                "SIMD" => return self.eval_simd_construction(param_args, args, env),
                "Error" => {
                    let msg = match self.eval_expr(&args[0], env)? {
                        Value::Str(s) => s,
                        other => {
                            return Err(RuntimeError::TypeError(format!(
                                "Error() expects a String, got {}",
                                type_name(&other)
                            )));
                        }
                    };
                    return Ok(Value::Error(msg));
                }
                _ if Dtype::from_scalar_alias(name).is_some() => {
                    let dtype = Dtype::from_scalar_alias(name).unwrap();
                    return self.eval_simd_construction_with(dtype, 1, args, env);
                }
                _ if self.structs.borrow().contains_key(name) => {
                    return self.eval_construction(name, param_args, args, env);
                }
                _ => return Err(RuntimeError::UndefinedVariable(name.to_string())),
            },
        };
        let closure = match callee {
            Value::Closure(c) => c,
            _ => return Err(RuntimeError::NotCallable(name.to_string())),
        };

        // Strict, call-by-value: fully evaluate the positional and keyword
        // argument values (in the caller's scope) before entering the call.
        let mut arg_values = Vec::with_capacity(args.len());
        for arg in args {
            arg_values.push(self.eval_expr(arg, env)?);
        }
        let mut kw_values = Vec::with_capacity(kwargs.len());
        for kw in kwargs {
            kw_values.push(self.eval_expr(&kw.value, env)?);
        }

        // Separate the regular parameters from a trailing `*args` (mirrors the
        // checker), and match arguments to the regular slots.
        let nreg = closure
            .params
            .iter()
            .position(|p| p.kind == crate::ast::ParamKind::Variadic)
            .unwrap_or(closure.params.len());
        let regular = &closure.params[..nreg];
        let variadic_param = closure.params.get(nreg); // present ⇒ the trailing `*args`
        let names: Vec<String> = regular.iter().map(|p| p.name.clone()).collect();
        let required = regular.iter().take_while(|p| p.default.is_none()).count();
        let kw_names: Vec<&str> = kwargs.iter().map(|k| k.name.as_str()).collect();
        let (slots, overflow) = crate::checker::match_call_slots(
            &names,
            regular.len(),
            required,
            args.len(),
            &kw_names,
            variadic_param.is_some(),
        )
        .map_err(|e| e.into_runtime_error(name))?;

        // Evaluate explicit value-parameter arguments (type args are erased).
        let value_params = self.eval_value_params(&closure.param_decls, param_args, env)?;

        // The call frame's parent is the closure's captured scope, not the
        // caller's — that is what makes scoping lexical rather than dynamic.
        let call_env = Scope::child(&closure.env);
        for (pname, value) in value_params {
            define(&call_env, &pname, value);
        }
        // Bind each regular parameter from its matched slot, evaluating a default
        // (in the closure's captured scope) where no argument was supplied.
        for (i, slot) in slots.iter().enumerate() {
            let param = &regular[i];
            let value = match slot {
                crate::checker::ArgSlot::Positional(p) => arg_values[*p].clone(),
                crate::checker::ArgSlot::Keyword(k) => kw_values[*k].clone(),
                crate::checker::ArgSlot::Default => match &param.default {
                    Some(default) => self.eval_expr(default, &closure.env)?,
                    None => {
                        return Err(RuntimeError::ArityMismatch {
                            name: name.to_string(),
                            expected: regular.len(),
                            got: args.len() + kwargs.len(),
                        });
                    }
                },
            };
            define(&call_env, &param.name, coerce(value, &param.ty));
        }
        // Collect any overflow positional arguments into the `*args` list, coercing
        // each element to the declared element type.
        if let Some(vp) = variadic_param {
            let items: Vec<Value> = overflow
                .iter()
                .map(|&idx| coerce(arg_values[idx].clone(), &vp.ty))
                .collect();
            define(&call_env, &vp.name, Value::List(items));
        }

        match self.exec_block(&closure.body, &call_env)? {
            Flow::Return(value) => Ok(value),
            // Falling off the end yields None; a stray break/continue (which the
            // checker rejects) is treated the same.
            _ => Ok(Value::None),
        }
    }

    /// The built-in `print(...)`: writes its arguments (each via `Display`),
    /// separated by a space and followed by a newline, to the output buffer.
    fn eval_print(&self, args: &[Expr], env: &Env) -> Result<Value, RuntimeError> {
        let mut line = String::new();
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                line.push(' ');
            }
            line.push_str(&self.eval_expr(arg, env)?.to_string());
        }
        line.push('\n');
        self.output.borrow_mut().push_str(&line);
        Ok(Value::None)
    }

    /// The built-in `abs(x)`: absolute value, preserving the numeric type.
    fn eval_abs(&self, args: &[Expr], env: &Env) -> Result<Value, RuntimeError> {
        match self.eval_expr(&args[0], env)? {
            Value::Int(n) => Ok(Value::Int(n.wrapping_abs())),
            Value::Float64(x) => Ok(Value::Float64(x.abs())),
            Value::UInt(n) => Ok(Value::UInt(n)), // already non-negative
            other => Err(RuntimeError::TypeError(format!(
                "abs() expects a numeric value, got {}",
                type_name(&other)
            ))),
        }
    }

    /// The built-in `min(a, b)` / `max(a, b)`: promote the operands to a common
    /// numeric kind (as arithmetic does) and return the smaller/larger.
    fn eval_min_max(&self, is_min: bool, args: &[Expr], env: &Env) -> Result<Value, RuntimeError> {
        let a = self.eval_expr(&args[0], env)?;
        let b = self.eval_expr(&args[1], env)?;
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
        // Compare in the promoted kind; rebuild the result in that kind.
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

    /// The built-in `range(stop)` / `range(start, stop)` / `range(start, stop,
    /// step)`. Arguments must be `Int`; a zero `step` is a runtime error.
    fn eval_range(&self, args: &[Expr], env: &Env) -> Result<Value, RuntimeError> {
        let mut ints = Vec::with_capacity(args.len());
        for arg in args {
            match self.eval_expr(arg, env)? {
                Value::Int(n) => ints.push(n),
                other => {
                    return Err(RuntimeError::TypeError(format!(
                        "range() expects Int arguments, got {}",
                        type_name(&other)
                    )));
                }
            }
        }
        let (start, stop, step) = match ints.as_slice() {
            [stop] => (0, *stop, 1),
            [start, stop] => (*start, *stop, 1),
            [start, stop, step] => (*start, *stop, *step),
            _ => {
                return Err(RuntimeError::TypeError(format!(
                    "range() takes 1 to 3 arguments, got {}",
                    ints.len()
                )));
            }
        };
        if step == 0 {
            return Err(RuntimeError::TypeError("range() step must not be zero".to_string()));
        }
        Ok(Value::Range { start, stop, step })
    }

    /// The numeric conversion built-ins `Int(x)` / `UInt(x)` / `Float64(x)`. The
    /// argument is one numeric or `Bool` value; the conversion follows Mojo
    /// (`Float64`→integer truncates toward zero, `Bool` is 0/1).
    fn eval_conversion(&self, name: &str, args: &[Expr], env: &Env) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(RuntimeError::ArityMismatch {
                name: name.to_string(),
                expected: 1,
                got: args.len(),
            });
        }
        let value = self.eval_expr(&args[0], env)?;
        // Read the operand as the three numeric "channels" we can target.
        let (as_i, as_u, as_f) = match value {
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

    /// Record a struct definition: its field order and its methods (each a
    /// `Closure` capturing `env`, the scope where the `struct` was defined).
    fn register_struct(
        &self,
        name: &str,
        type_params: &[crate::ast::TypeParam],
        fields: &[Param],
        methods: &[Method],
        fieldwise_init: bool,
        env: &Env,
    ) {
        let fields = fields.iter().map(|f| (f.name.clone(), f.ty.clone())).collect();
        let mut method_map = HashMap::new();
        for m in methods {
            method_map.insert(
                m.name.clone(),
                Rc::new(Closure {
                    params: m.params.clone(),
                    param_decls: Vec::new(), // methods take no compile-time params
                    body: m.body.clone(),
                    env: env.clone(),
                    mut_self: matches!(m.self_convention, Some(crate::ast::ArgConvention::Mut)),
                }),
            );
        }
        self.structs.borrow_mut().insert(
            name.to_string(),
            Rc::new(StructDef {
                fields,
                param_decls: classify_param_decls(type_params),
                methods: method_map,
                fieldwise_init,
            }),
        );
    }

    /// Construct a struct via its fieldwise constructor: `Name[params](v1, ...)`,
    /// reifying any explicit value parameters onto the instance.
    fn eval_construction(&self, name: &str, param_args: &[crate::ast::ParamArg], args: &[Expr], env: &Env) -> Result<Value, RuntimeError> {
        let def = self.structs.borrow().get(name).cloned().expect("caller checked");
        if !def.fieldwise_init {
            return Err(RuntimeError::TypeError(format!(
                "struct '{}' has no constructor",
                name
            )));
        }
        if def.fields.len() != args.len() {
            return Err(RuntimeError::ArityMismatch {
                name: name.to_string(),
                expected: def.fields.len(),
                got: args.len(),
            });
        }
        let value_params = self.eval_value_params(&def.param_decls, param_args, env)?;
        let mut fields = Vec::with_capacity(args.len());
        for ((fname, fty), arg) in def.fields.iter().zip(args) {
            fields.push((fname.clone(), coerce(self.eval_expr(arg, env)?, fty)));
        }
        Ok(Value::Struct { name: name.to_string(), fields, value_params })
    }

    /// Evaluate the explicit value-parameter arguments of a generic use site,
    /// pairing each value parameter (by declaration position) with its argument
    /// expression. Type-parameter arguments are erased. Returns an empty list
    /// when no explicit `[...]` was given (there are then no value parameters).
    fn eval_value_params(
        &self,
        param_decls: &[(String, bool)],
        param_args: &[crate::ast::ParamArg],
        env: &Env,
    ) -> Result<Vec<(String, Value)>, RuntimeError> {
        use crate::ast::ParamArg;
        let mut out = Vec::new();
        for ((pname, is_value), arg) in param_decls.iter().zip(param_args) {
            if *is_value {
                let value = match arg {
                    ParamArg::Value(expr) => self.eval_expr(expr, env)?,
                    // The checker guarantees a value parameter gets a value.
                    ParamArg::Type(_) => Value::None,
                };
                out.push((pname.clone(), value));
            }
        }
        Ok(out)
    }

    /// Construct a `List`: `List[T](args)` coerces each element to `T`;
    /// `List(args)` promotes numeric elements to a common kind (element-type
    /// inference). Value semantics: elements are owned by the new list.
    fn eval_list_construction(&self, param_args: &[crate::ast::ParamArg], args: &[Expr], env: &Env) -> Result<Value, RuntimeError> {
        use crate::ast::ParamArg;
        // An explicit `List[T]` gives the element type to coerce each element to.
        let elem_ty: Option<Type> = match param_args.first() {
            Some(ParamArg::Type(t)) => Some(t.clone()),
            Some(ParamArg::Value(Expr::Identifier(id))) => Some(Type::Named(id.clone(), vec![])),
            _ => None,
        };
        if let Some(ty) = elem_ty {
            let mut items = Vec::with_capacity(args.len());
            for arg in args {
                items.push(coerce(self.eval_expr(arg, env)?, &ty));
            }
            Ok(Value::List(items))
        } else {
            Ok(Value::List(self.eval_list_elems(args, env)?))
        }
    }

    /// Construct a `SIMD[DType.<dt>, width](args)` value from its parameter and
    /// element arguments.
    fn eval_simd_construction(&self, param_args: &[crate::ast::ParamArg], args: &[Expr], env: &Env) -> Result<Value, RuntimeError> {
        use crate::ast::ParamArg;
        // param_args = [DType.<dt>, width]; the checker validated their shape.
        let dtype = match param_args.first() {
            Some(ParamArg::Value(Expr::Member { field, .. })) => Dtype::from_name(field)
                .ok_or_else(|| RuntimeError::TypeError(format!("bad dtype '{}'", field)))?,
            _ => return Err(RuntimeError::TypeError("SIMD needs a DType".to_string())),
        };
        let width = match param_args.get(1) {
            Some(ParamArg::Value(expr)) => match self.eval_expr(expr, env)? {
                Value::Int(w) => w,
                other => {
                    return Err(RuntimeError::TypeError(format!(
                        "SIMD width must be Int, got {}",
                        type_name(&other)
                    )));
                }
            },
            _ => return Err(RuntimeError::TypeError("SIMD needs a width".to_string())),
        };
        self.eval_simd_construction_with(dtype, width, args, env)
    }

    /// Build a SIMD value of `dtype` and `width` from element arguments: exactly
    /// `width` elements (one per lane) or a single element splatted to all lanes.
    fn eval_simd_construction_with(&self, dtype: Dtype, width: i64, args: &[Expr], env: &Env) -> Result<Value, RuntimeError> {
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.eval_expr(arg, env)?);
        }
        let width = width as usize;
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

    /// Snapshot of the global scope's bindings, sorted by name (for display/tests).
    pub fn global_bindings(&self) -> Vec<(String, Value)> {
        let mut bindings: Vec<(String, Value)> = self
            .global
            .borrow()
            .vars
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        bindings.sort_by(|(a, _), (b, _)| a.cmp(b));
        bindings
    }
}

impl Default for Evaluator {
    fn default() -> Self {
        Self::new()
    }
}
