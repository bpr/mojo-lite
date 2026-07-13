use super::*;

pub(super) fn unify(
    pattern: &Ty,
    actual: &Ty,
    subst: &mut HashMap<String, Ty>,
) -> Result<(), TypeError> {
    match pattern {
        Ty::Param { name, .. } => {
            let solved = default_literal(actual);
            match subst.get(name) {
                None => {
                    subst.insert(name.clone(), solved);
                }
                Some(existing) if *existing == solved => {}
                Some(existing) => {
                    return Err(TypeError::TypeMismatch {
                        expected: existing.to_string(),
                        found: solved.to_string(),
                        context: format!("type parameter '{}'", name),
                    });
                }
            }
            Ok(())
        }
        // Recurse into a parameterized struct pattern to solve nested type
        // parameters (`Pair[T]` against `Pair[Int]` solves `T = Int`). Value
        // arguments contribute no type solution. A structural mismatch is left
        // for the caller's coercion check to report.
        Ty::Struct(pn, pargs) => {
            if let Ty::Struct(an, aargs) = actual
                && pn == an
                && pargs.len() == aargs.len()
            {
                for (p, a) in pargs.iter().zip(aargs) {
                    if let (TyArg::Ty(p), TyArg::Ty(a)) = (p, a) {
                        unify(p, a, subst)?;
                    }
                }
            }
            Ok(())
        }
        // A non-parameter pattern contributes no solution; coercion is checked
        // separately by the caller.
        _ => Ok(()),
    }
}

/// Replace every `Ty::Param` in `ty` with its solution from `subst` (leaving an
/// unsolved parameter untouched). Recurses into struct type arguments.
pub(super) fn substitute(ty: &Ty, subst: &HashMap<String, Ty>) -> Ty {
    match ty {
        Ty::Param { name, .. } => subst.get(name).cloned().unwrap_or_else(|| ty.clone()),
        Ty::Struct(name, args) => {
            Ty::Struct(name.clone(), map_tyargs(args, |t| substitute(t, subst)))
        }
        Ty::List(elem) => Ty::List(Box::new(substitute(elem, subst))),
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|t| substitute(t, subst)).collect()),
        Ty::Pointer(elem) => Ty::Pointer(Box::new(substitute(elem, subst))),
        Ty::Assoc { base, name } => Ty::Assoc {
            base: Box::new(substitute(base, subst)),
            name: name.clone(),
        },
        Ty::Func {
            params,
            names,
            ret,
            required,
            variadic,
            positional_only,
            keyword_only,
            conventions,
        } => Ty::Func {
            params: params.iter().map(|p| substitute(p, subst)).collect(),
            names: names.clone(),
            ret: Box::new(substitute(ret, subst)),
            required: required.clone(),
            variadic: variadic.as_ref().map(|v| Box::new(substitute(v, subst))),
            positional_only: *positional_only,
            keyword_only: *keyword_only,
            conventions: conventions.clone(),
        },
        Ty::Overload(candidates) => Ty::Overload(
            candidates
                .iter()
                .map(|candidate| substitute(candidate, subst))
                .collect(),
        ),
        _ => ty.clone(),
    }
}

/// Replace every `Ty::SelfType` in `ty` with `replacement` (the conforming
/// struct type, or a bounded `T`). Recurses into struct/function types.
pub(super) fn substitute_self(ty: &Ty, replacement: &Ty) -> Ty {
    match ty {
        Ty::SelfType => replacement.clone(),
        Ty::Struct(name, args) => Ty::Struct(
            name.clone(),
            map_tyargs(args, |t| substitute_self(t, replacement)),
        ),
        Ty::List(elem) => Ty::List(Box::new(substitute_self(elem, replacement))),
        Ty::Tuple(elems) => Ty::Tuple(
            elems
                .iter()
                .map(|t| substitute_self(t, replacement))
                .collect(),
        ),
        Ty::Pointer(elem) => Ty::Pointer(Box::new(substitute_self(elem, replacement))),
        Ty::Assoc { base, name } => Ty::Assoc {
            base: Box::new(substitute_self(base, replacement)),
            name: name.clone(),
        },
        Ty::Func {
            params,
            names,
            ret,
            required,
            variadic,
            positional_only,
            keyword_only,
            conventions,
        } => Ty::Func {
            params: params
                .iter()
                .map(|p| substitute_self(p, replacement))
                .collect(),
            names: names.clone(),
            ret: Box::new(substitute_self(ret, replacement)),
            required: required.clone(),
            variadic: variadic
                .as_ref()
                .map(|v| Box::new(substitute_self(v, replacement))),
            positional_only: *positional_only,
            keyword_only: *keyword_only,
            conventions: conventions.clone(),
        },
        Ty::Overload(candidates) => Ty::Overload(
            candidates
                .iter()
                .map(|candidate| substitute_self(candidate, replacement))
                .collect(),
        ),
        _ => ty.clone(),
    }
}

/// Apply `f` to each type argument of a struct's parameter list, passing value
/// arguments through unchanged.
pub(super) fn map_tyargs(args: &[TyArg], mut f: impl FnMut(&Ty) -> Ty) -> Vec<TyArg> {
    args.iter()
        .map(|a| match a {
            TyArg::Ty(t) => TyArg::Ty(f(t)),
            TyArg::Val(v) => TyArg::Val(v.clone()),
        })
        .collect()
}
