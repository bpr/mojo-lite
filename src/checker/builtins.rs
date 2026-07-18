//! Typing and capability rules for compiler-known builtin types and operations.

use super::*;

pub(super) fn default_literal(ty: &Ty) -> Ty {
    match ty {
        Ty::IntLiteral => Ty::Int,
        Ty::FloatLiteral => Ty::Float64,
        // Materialize each element of a tuple literal (`(1, 2)` → `Tuple[Int, Int]`).
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(default_literal).collect()),
        Ty::Variant(alternatives) => {
            Ty::Variant(alternatives.iter().map(default_literal).collect())
        }
        other => other.clone(),
    }
}

/// Whether `ty` is a non-numeric scalar value type — what `==`/`!=` compare once
/// the numeric cases (handled by `common_numeric`) are out of the way.
pub(super) fn is_scalar(ty: &Ty) -> bool {
    matches!(ty, Ty::Bool | Ty::String | Ty::None)
}

/// Whether an opaque type parameter carries a bound that promises equality.
/// Built-in bounds are intentionally shallow today, but `T: Equatable` should at
/// least let generic library code type-check `T == T`. `Comparable` refines
/// equality (roadmap milestone 4), so it counts too; `Hashable` deliberately does **not**
/// (a hash-backed key bounds `K: Hashable & Equatable` when it needs both).
pub(super) fn has_equality_bound(ty: &Ty) -> bool {
    match ty {
        Ty::Param { bounds, .. } => bounds.iter().any(|b| {
            matches!(
                b.as_str(),
                "Equatable" | "Comparable" | "EqualityComparable"
            )
        }),
        _ => false,
    }
}

pub(super) fn has_equality_bound_or_concrete(checker: &Checker, ty: &Ty) -> bool {
    match ty {
        Ty::Struct(name, _) => checker
            .structs
            .get(name)
            .is_some_and(|s| s.conforms.iter().any(|c| c == "Equatable")),
        Ty::Variant(alternatives) => alternatives
            .iter()
            .all(|alternative| has_equality_bound_or_concrete(checker, alternative)),
        _ => has_equality_bound(ty) || is_scalar(ty) || is_numeric_like(ty),
    }
}

/// Whether an opaque type parameter carries a bound that promises an ordering
/// (`<`/`<=`/`>`/`>=`). Only `Comparable` grants this — a plain `T: Equatable`
/// permits `==`/`!=` but *not* ordering (see `has_equality_bound`). In current
/// Mojo `Comparable` also implies equality, which `has_equality_bound` reflects.
pub(super) fn has_order_bound(ty: &Ty) -> bool {
    match ty {
        Ty::Param { bounds, .. } => bounds.iter().any(|b| b.as_str() == "Comparable"),
        _ => false,
    }
}

/// Whether an opaque type parameter carries a bound that promises a length, so
/// `len(x)` is well-typed on it. `Sized` (`__len__(self) -> Int`) and
/// `SizedRaising` (`__len__(self) raises -> Int`) both do — mojito's effect
/// analysis is deferred, so the two are not distinguished at the call site; a
/// plain `T: AnyType` grants no length.
pub(super) fn has_len_bound(ty: &Ty) -> bool {
    match ty {
        Ty::Param { bounds, .. } => bounds
            .iter()
            .any(|b| matches!(b.as_str(), "Sized" | "SizedRaising")),
        _ => false,
    }
}

/// Whether `ty` is an opaque type parameter carrying the named trait `bound`.
/// The numeric-operation traits (roadmap milestone 7 — `Absable`/`Roundable`/`Powable`/
/// `Intable`/`Floatable`/`Boolable`/`DivModable`) gate a corresponding built-in
/// or operator on an opaque `T` this way: the concrete type's implementation
/// runs after type erasure.
pub(super) fn param_has_bound(ty: &Ty, bound: &str) -> bool {
    matches!(ty, Ty::Param { bounds, .. } if bounds.iter().any(|b| b == bound))
}

pub(super) fn builtin_trait_operation(trait_name: &str) -> Option<&'static str> {
    match trait_name {
        "Hashable" => Some("__hash__() -> UInt"),
        "Absable" => Some("__abs__() -> Self"),
        "Roundable" => Some("__round__() -> Self"),
        "Powable" => Some("__pow__(Self) -> Self"),
        "Intable" => Some("__int__() -> Int"),
        "Floatable" => Some("__float__() -> Float64"),
        "Boolable" => Some("__bool__() -> Bool"),
        "DivModable" => Some("__divmod__(Self) -> Tuple[Self, Self]"),
        _ => None,
    }
}

/// The trait bounds that supply a numeric-rounding dunder (`method`/`argc`),
/// used by the self-hosted `math` module (roadmap milestone 7). `__floor__`/`__ceil__`/
/// `__trunc__` are nullary (`Floorable`/`Ceilable`/`Truncable`); `__ceildiv__`
/// is unary and granted by `CeilDivable` or its raising sibling
/// `CeilDivableRaising` (mojito's deferred effect model does not distinguish
/// them). A bound satisfies the dunder if it is any of the returned names.
pub(super) fn math_dunder_bound(method: &str, argc: usize) -> &'static [&'static str] {
    match (method, argc) {
        ("__floor__", 0) => &["Floorable"],
        ("__ceil__", 0) => &["Ceilable"],
        ("__trunc__", 0) => &["Truncable"],
        ("__ceildiv__", 1) => &["CeilDivable", "CeilDivableRaising"],
        _ => &[],
    }
}

/// Whether a *concrete* built-in type has an intrinsic `__hash__` — the scalar
/// set the VM can hash directly (`Int`/`UInt`/`Bool`/`String`/`Float64`). This
/// lets a user key struct combine `self.field.__hash__()` values.
pub(super) fn builtin_hashable_ty(ty: &Ty) -> bool {
    matches!(ty, Ty::Int | Ty::UInt | Ty::Bool | Ty::String | Ty::Float64)
}

pub(super) fn is_numeric_like(ty: &Ty) -> bool {
    is_numeric(&default_literal(ty))
}

/// Enforce that a builtin-driven dunder (`__len__`/`__str__`/`__contains__`)
/// returns its Mojo-mandated type, so `len`/`String`/`in` on a user struct stay
/// well-typed.
pub(super) fn require_dunder_ret(ret: Ty, expected: &Ty, name: &str) -> Result<Ty, TypeError> {
    if ret == *expected {
        Ok(ret)
    } else {
        Err(TypeError::TypeMismatch {
            expected: expected.to_string(),
            found: ret.to_string(),
            context: format!("return type of '{name}'"),
        })
    }
}

/// Whether list elements of type `ty` can be compared for equality (needed by
/// `List.remove`/`count`/`index`) — the same scalar set `==`/`!=` accept.
pub(super) fn is_list_equatable(ty: &Ty) -> bool {
    is_numeric(ty) || matches!(ty, Ty::Bool | Ty::String | Ty::None) || has_equality_bound(ty)
}

/// Whether every element in a tuple supports equality. Tuples recurse so nested
/// tuple comparisons and membership stay structural without making `List`
/// equality part of this compiler-known subset.
pub(super) fn tuple_elements_equatable(elements: &[Ty]) -> bool {
    elements.iter().all(|ty| match ty {
        Ty::Tuple(nested) => tuple_elements_equatable(nested),
        other => is_list_equatable(other),
    })
}

fn tuple_element_comparable(ty: &Ty) -> bool {
    match ty {
        Ty::Tuple(nested) => tuple_elements_comparable(nested),
        Ty::String => true,
        other => is_numeric(other) || has_order_bound(other),
    }
}

fn tuple_elements_comparable(elements: &[Ty]) -> bool {
    elements.iter().all(tuple_element_comparable)
}

fn tuple_order_pair_compatible(left: &Ty, right: &Ty) -> bool {
    if common_numeric(left, right).is_some() {
        return true;
    }
    match (left, right) {
        (Ty::String, Ty::String) => true,
        (Ty::Tuple(left), Ty::Tuple(right)) => tuple_order_compatible(left, right),
        _ => left == right && has_order_bound(left),
    }
}

/// Tuple ordering is lexicographic. Every element must be comparable, and each
/// pair in the common prefix must have a compatible comparison operation.
pub(super) fn tuple_order_compatible(left: &[Ty], right: &[Ty]) -> bool {
    tuple_elements_comparable(left)
        && tuple_elements_comparable(right)
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| tuple_order_pair_compatible(left, right))
}

/// Whether a value of type `ty` can be `print`ed (has a user-facing display).
/// Functions, ranges, and opaque type parameters are not printable.
pub(super) fn is_printable(ty: &Ty) -> bool {
    match ty {
        Ty::Int
        | Ty::UInt
        | Ty::Bool
        | Ty::String
        | Ty::Float64
        | Ty::None
        | Ty::IntLiteral
        | Ty::FloatLiteral
        | Ty::Struct(_, _)
        | Ty::Simd { .. }
        | Ty::Error
        | Ty::List(_)
        | Ty::Set(_)
        | Ty::Dict(_, _) => true,
        // A tuple prints if every element prints.
        Ty::Tuple(elems) => elems.iter().all(is_printable),
        Ty::Variant(alternatives) => alternatives.iter().all(is_printable),
        _ => false,
    }
}

/// Whether `ty` is a numeric type (concrete or literal).
pub(super) fn is_numeric(ty: &Ty) -> bool {
    matches!(
        ty,
        Ty::Int | Ty::UInt | Ty::Float64 | Ty::IntLiteral | Ty::FloatLiteral
    )
}

/// Whether a value of type `from` can be used where `to` is required. Only the
/// literal types coerce (to the concrete numeric types, or `IntLiteral` up to
/// `FloatLiteral`); everything else must match exactly.
pub(super) fn coerces(from: &Ty, to: &Ty) -> bool {
    if *from == Ty::Never {
        return true;
    }
    if from == to {
        return true;
    }
    match (from, to) {
        (Ty::Struct(from, from_args), Ty::Struct(to, to_args))
            if matches!(from.as_str(), "ContiguousSlice" | "StridedSlice")
                && to == "Slice"
                && from_args.is_empty()
                && to_args.is_empty() =>
        {
            true
        }
        (Ty::Param { name: a, .. }, Ty::Param { name: b, .. }) => a == b,
        (Ty::Struct(an, aargs), Ty::Struct(bn, bargs)) => {
            an == bn
                && aargs.len() == bargs.len()
                && aargs.iter().zip(bargs).all(|(a, b)| match (a, b) {
                    (TyArg::Ty(a), TyArg::Ty(b)) => coerces(a, b),
                    (TyArg::Val(a), TyArg::Val(b)) => a == b,
                    _ => false,
                })
        }
        (Ty::List(a), Ty::List(b)) => coerces(a, b),
        (Ty::Set(a), Ty::Set(b)) => coerces(a, b),
        (Ty::Dict(ak, av), Ty::Dict(bk, bv)) => coerces(ak, bk) && coerces(av, bv),
        (
            Ty::Pointer {
                element: a,
                origin: ao,
            },
            Ty::Pointer {
                element: b,
                origin: bo,
            },
        ) => coerces(a, b) && ao == bo,
        (
            Ty::Func {
                params: from_params,
                ret: from_ret,
                required,
                variadic,
                conventions,
                raises: from_raises,
                error: from_error,
                ..
            },
            Ty::Func {
                params: to_params,
                ret: to_ret,
                required: to_required,
                variadic: to_variadic,
                conventions: to_conventions,
                raises: to_raises,
                error: to_error,
                ..
            },
        ) => {
            required == to_required
                && variadic.is_none()
                && to_variadic.is_none()
                && conventions == to_conventions
                && (!from_raises || *to_raises)
                && match (from_error.as_deref(), to_error.as_deref()) {
                    (None, None) => true,
                    (None, Some(Ty::Never)) => true,
                    (None, Some(_)) => true,
                    (Some(from), Some(Ty::Error)) => from != &Ty::Never,
                    (Some(from), Some(to)) => from == to,
                    (Some(Ty::Never), None) => true,
                    (Some(_), None) => false,
                }
                && from_params.len() == to_params.len()
                && from_params
                    .iter()
                    .zip(to_params)
                    .all(|(from, to)| from == to)
                && from_ret == to_ret
        }
        (Ty::IntLiteral, Ty::Int | Ty::UInt | Ty::Float64 | Ty::FloatLiteral) => true,
        (Ty::FloatLiteral, Ty::Float64) => true,
        (literal, Ty::Simd { dtype, width: 1 }) if splats_to(literal, *dtype) => true,
        (
            Ty::Simd {
                dtype: from_dtype,
                width: from_width,
            },
            Ty::Simd {
                dtype: to_dtype,
                width: -1,
            },
        ) => from_dtype == to_dtype && *from_width > 0,
        // A tuple coerces element-wise (same arity) — so a literal element
        // materializes: `(1, 2.0)` fits `Tuple[Float64, Float64]`.
        (Ty::Tuple(a), Ty::Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| coerces(x, y))
        }
        (Ty::Variant(a), Ty::Variant(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| coerces(x, y))
        }
        _ => false,
    }
}

/// The common type of two list elements: numeric elements unify like operands
/// (widening literals); otherwise the two must be equal.
pub(super) fn common_elem(a: &Ty, b: &Ty) -> Option<Ty> {
    if is_numeric(a) && is_numeric(b) {
        common_numeric(a, b)
    } else if a == b {
        Some(a.clone())
    } else {
        None
    }
}

/// The common type of two numeric operands, coercing literals as needed, or
/// `None` if they can't be unified (e.g. two different concrete types).
/// The common type of a ternary's two branches: unify numerics (widening
/// literals), else an exact match or a one-way literal coercion. `None` if the
/// branches are incompatible.
pub(super) fn common_branch_ty(a: &Ty, b: &Ty) -> Option<Ty> {
    if let Some(c) = common_numeric(a, b) {
        return Some(c);
    }
    if a == b {
        Some(a.clone())
    } else if coerces(a, b) {
        Some(b.clone())
    } else if coerces(b, a) {
        Some(a.clone())
    } else {
        None
    }
}

pub(super) fn common_numeric(a: &Ty, b: &Ty) -> Option<Ty> {
    if !is_numeric(a) || !is_numeric(b) {
        return None;
    }
    if a == b {
        Some(a.clone())
    } else if coerces(a, b) {
        Some(b.clone())
    } else if coerces(b, a) {
        Some(a.clone())
    } else {
        None
    }
}
