use super::*;

pub(super) fn dtype_from_arg(arg: &crate::ast::ParamArg) -> Result<Dtype, TypeError> {
    if let crate::ast::ParamArg::Value(Expr {
        kind: ExprKind::Member { object, field },
        ..
    }) = arg
        && let ExprKind::Identifier(ns) = &object.kind
        && ns == "DType"
        && let Some(dtype) = Dtype::from_name(field)
    {
        return Ok(dtype);
    }
    Err(TypeError::BadDtype(match arg {
        crate::ast::ParamArg::Value(Expr {
            kind: ExprKind::Member { field, .. },
            ..
        }) => {
            format!("DType.{}", field)
        }
        _ => "a non-DType argument".to_string(),
    }))
}

/// Whether a value of type `ty` can be a `dtype` SIMD element (a construction
/// argument, or the non-SIMD operand of an elementwise operator that splats). A
/// numeric literal fits any matching-kind lane; a same-dtype width-1 SIMD fits.
pub(super) fn splats_to(ty: &Ty, dtype: Dtype) -> bool {
    match ty {
        Ty::IntLiteral => dtype != Dtype::Bool,
        Ty::FloatLiteral => dtype.is_float(),
        Ty::Bool => dtype == Dtype::Bool,
        // `Float64` is `SIMD[DType.float64, 1]`, so it splats into a float64 vector.
        Ty::Float64 => dtype == Dtype::Float64,
        Ty::Simd { dtype: d, width: 1 } => *d == dtype,
        _ => false,
    }
}

/// The (canonicalized) `Ty` for a SIMD of `dtype`/`width`: a **width-1 `float64`**
/// is the native `Ty::Float64` (Mojo unifies `Float64` with `SIMD[DType.float64,
/// 1]`); everything else is a `Ty::Simd`.
pub(super) fn simd_ty(dtype: Dtype, width: i64) -> Ty {
    if dtype == Dtype::Float64 && width == 1 {
        Ty::Float64
    } else {
        Ty::Simd { dtype, width }
    }
}

/// The scalar `Ty` a value-parameter type name denotes, or `None` if the name is
/// not a scalar type (so it is a trait, i.e. a type parameter). Used to classify
/// `[name: X]` as a value vs. type parameter.
pub(super) fn scalar_type_name(name: &str) -> Option<Ty> {
    match name {
        "Int" => Some(Ty::Int),
        "UInt" => Some(Ty::UInt),
        "Bool" => Some(Ty::Bool),
        "String" => Some(Ty::String),
        "Float64" => Some(Ty::Float64),
        _ => None,
    }
}

/// The type-parameter scope (`name → bounds`) of a parameter list, for resolving
/// a bare `T` annotation. Value parameters are excluded (they are `Int` locals).
pub(super) fn type_scope(decls: &[ParamDecl]) -> HashMap<String, Vec<String>> {
    decls
        .iter()
        .filter_map(|d| match d {
            ParamDecl::Type { name, bounds } => Some((name.clone(), bounds.clone())),
            ParamDecl::Value { .. } => None,
        })
        .collect()
}

/// A struct's own parameter, as the `TyArg` it contributes to the struct's `Self`
/// type while its body is checked: a type parameter as `Ty::Param`, a value
/// parameter as a symbolic `CtValue::Param`.
pub(super) fn param_as_arg(decl: &ParamDecl) -> TyArg {
    match decl {
        ParamDecl::Type { name, bounds } => TyArg::Ty(Ty::Param {
            name: name.clone(),
            bounds: bounds.clone(),
        }),
        ParamDecl::Value { name } => TyArg::Val(CtValue::Param(name.clone())),
    }
}

/// The substitution mapping a struct's type-parameter names to a value's type
/// arguments (`[T] @ [Int]` ⟹ `{T: Int}`). Value parameters/arguments are
/// skipped (they never appear in a type). Empty for a non-generic struct.
pub(super) fn struct_subst(decls: &[ParamDecl], targs: &[TyArg]) -> HashMap<String, Ty> {
    decls
        .iter()
        .zip(targs)
        .filter_map(|(d, a)| match (d, a) {
            (ParamDecl::Type { name, .. }, TyArg::Ty(t)) => Some((name.clone(), t.clone())),
            _ => None,
        })
        .collect()
}
