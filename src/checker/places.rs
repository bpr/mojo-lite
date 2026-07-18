//! Place construction, overlap, mutability, and origin derivation for checking.

use super::*;

pub(super) fn parameter_is_writable(convention: Option<ArgConvention>) -> bool {
    matches!(
        convention,
        Some(ArgConvention::Mut | ArgConvention::Ref | ArgConvention::Out)
    )
}

/// Solve a type parameter against an actual type, recording it in `subst`. A
/// numeric literal is defaulted to its concrete type first (`IntLiteral → Int`,
/// `FloatLiteral → Float64`) so the solution matches the value the VM stores —
/// this deliberately forbids widening one literal to match
/// another across arguments (e.g. `Pair(1.0, 2)` is a conflict, not `Pair[Float64]`).
/// Whether an expression is a **place** — it names an existing binding (a variable
/// or a field/index chain rooted at one) rather than producing a fresh value. A
/// `^` transfer, a call result, a literal, or an operator is *not* a place.
pub(super) fn is_place_expr(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Identifier(_)
            | ExprKind::Member { .. }
            | ExprKind::Index { .. }
            | ExprKind::TypeApply { .. }
    )
}

/// The root variable of a place expression (`p` for `p`, `p.a.b`, `p.items[i]`),
/// or `None` if the expression isn't rooted at a variable. A `mut`/shared borrow of
/// a place borrows its root, so the borrow checker keys on this.
/// Mojo's borrow rule (mutable-XOR-shared), checked per call and **place-sensitive**
/// (field-aware). An argument accesses its place either **exclusively** (a
/// `mut`/`ref` borrow, or a `^` move) or **shared** (a plain `read`/default borrow).
/// Any number of shared accesses to overlapping places is fine, but an exclusive
/// access requires no *overlapping* place elsewhere in the call — so `f(mut a, a)`,
/// `f(mut a, mut a)`, `f(a, a^)`, and `f(mut p, p.a)` are rejected, while
/// `f(mut p.a, mut p.b)` (disjoint fields) is allowed. mojito's borrows are
/// call-scoped (no references persist in variables), so this per-call check is
/// complete — no cross-block loan dataflow is needed.
pub(super) fn check_call_aliasing(
    slots: &[ArgSlot],
    conventions: &[Option<ArgConvention>],
    copied_reads: &[bool],
    args: &[Expr],
    kwargs: &[crate::ast::KwArg],
) -> Result<(), TypeError> {
    // Each place argument's access: its full place (root + projection path) and
    // whether it is *exclusive* (a `mut`/`ref` borrow, or a `^` move).
    let mut accesses: Vec<(&str, Vec<PlaceSeg>, bool, bool)> = Vec::new();
    for (i, slot) in slots.iter().enumerate() {
        let arg = match slot {
            ArgSlot::Positional(p) => &args[*p],
            ArgSlot::Keyword(k) => &kwargs[*k].value,
            ArgSlot::Default => continue,
        };
        let (place, exclusive) = match &arg.kind {
            ExprKind::Transfer(inner) => (place_path(inner), true),
            _ => (
                place_path(arg),
                matches!(
                    conventions.get(i),
                    Some(Some(ArgConvention::Mut | ArgConvention::Ref))
                ),
            ),
        };
        if let Some((root, path)) = place {
            accesses.push((
                root,
                path,
                exclusive,
                copied_reads.get(i).copied().unwrap_or(false),
            ));
        }
    }
    // Mutable-XOR-shared, **place-sensitive**: two accesses to the *same variable*
    // conflict only if their places overlap (a prefix relationship) and at least one
    // is exclusive. So `f(mut p.a, mut p.b)` is fine (disjoint fields), while
    // `f(mut p.a, p.a)` and `f(mut p, p.a)` are rejected.
    for i in 0..accesses.len() {
        for j in (i + 1)..accesses.len() {
            let (ra, pa, ea, ca) = &accesses[i];
            let (rb, pb, eb, cb) = &accesses[j];
            let live_alias_conflict = (*ea && !*cb) || (*eb && !*ca);
            if ra == rb && live_alias_conflict && places_overlap(pa, pb) {
                return Err(TypeError::AliasingViolation {
                    var: ra.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// One step of a place's projection path (used by the place-sensitive borrow
/// check). A dynamic `Index` is treated conservatively — it may alias any index.
pub(super) enum PlaceSeg {
    Field(String),
    Index,
}

/// A place expression's root variable and projection path (root → leaf), or `None`
/// if it isn't rooted at a variable.
pub(super) fn place_path(e: &Expr) -> Option<(&str, Vec<PlaceSeg>)> {
    fn go<'a>(e: &'a Expr, path: &mut Vec<PlaceSeg>) -> Option<&'a str> {
        match &e.kind {
            ExprKind::Identifier(n) => Some(n),
            ExprKind::Member { object, field } => {
                let r = go(object, path)?;
                path.push(PlaceSeg::Field(field.clone()));
                Some(r)
            }
            ExprKind::Index { object, .. } => {
                let r = go(object, path)?;
                path.push(PlaceSeg::Index);
                Some(r)
            }
            _ => None,
        }
    }
    let mut path = Vec::new();
    let root = go(e, &mut path)?;
    Some((root, path))
}

/// Whether two projection paths (of the same root) may refer to overlapping
/// memory: they overlap unless a `Field` step names distinct fields. A dynamic
/// `Index` conservatively may alias, so it never proves disjointness.
fn places_overlap(a: &[PlaceSeg], b: &[PlaceSeg]) -> bool {
    for (x, y) in a.iter().zip(b) {
        if let (PlaceSeg::Field(fa), PlaceSeg::Field(fb)) = (x, y)
            && fa != fb
        {
            return false;
        }
    }
    true
}
