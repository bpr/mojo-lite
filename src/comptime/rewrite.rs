use super::*;

// --- Substitution / materialization -----------------------------------------
//
// One generic rewrite over the AST parameterized by a name→value lookup, used for
// two things: substituting a `comptime for` loop variable with its literal (in the
// unrolled body — does NOT descend into nested `def`/`struct`), and materializing
// module-level `comptime` constants into runtime literals (does descend, minus a
// function's own parameter names, which shadow).

/// A name→compile-time-value lookup for a rewrite.
pub(super) type Subs<'a> = &'a dyn Fn(&str) -> Option<CtValue>;

/// Materialize module-level comptime constants throughout a program.
pub(super) fn materialize_block(stmts: Vec<Stmt>, consts: &HashMap<String, CtValue>) -> Vec<Stmt> {
    let subs: Subs = &|n| consts.get(n).cloned();
    stmts
        .into_iter()
        .map(|s| rewrite_stmt_cloned(&s, subs, true))
        .collect()
}

pub(super) fn rewrite_stmt_cloned(s: &Stmt, subs: Subs, into_defs: bool) -> Stmt {
    let mut s = s.clone();
    rewrite_stmt(&mut s, subs, into_defs);
    s
}

fn rewrite_expr(e: &mut Expr, subs: Subs) {
    match &mut e.kind {
        ExprKind::Identifier(name) => {
            if let Some(v) = subs(name)
                && let Some(materialized) = v.materialize(e.span)
            {
                *e = materialized;
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::None
        | ExprKind::TString { .. } => {}
        // A value **parameter** argument (`Box[CAP](…)`, `UnsafePointer[CAP]`) may
        // reference a comptime constant, so rewrite the `Value` param args too.
        ExprKind::TypeApply { args, .. } => rewrite_param_args(args, subs),
        ExprKind::Prefix(_, inner) | ExprKind::Transfer(inner) => rewrite_expr(inner, subs),
        ExprKind::Infix(_, l, r) => {
            rewrite_expr(l, subs);
            rewrite_expr(r, subs);
        }
        ExprKind::Compare { first, rest } => {
            rewrite_expr(first, subs);
            for (_, r) in rest {
                rewrite_expr(r, subs);
            }
        }
        ExprKind::Call {
            param_args,
            args,
            kwargs,
            ..
        } => {
            rewrite_param_args(param_args, subs);
            rewrite_exprs(args, subs);
            for k in kwargs {
                rewrite_expr(&mut k.value, subs);
            }
        }
        ExprKind::Member { object, .. } => rewrite_expr(object, subs),
        ExprKind::MethodCall {
            object,
            args,
            kwargs,
            ..
        } => {
            rewrite_expr(object, subs);
            rewrite_exprs(args, subs);
            for k in kwargs {
                rewrite_expr(&mut k.value, subs);
            }
        }
        ExprKind::Index { object, index } => {
            rewrite_expr(object, subs);
            rewrite_expr(index, subs);
        }
        ExprKind::Slice {
            object,
            lower,
            upper,
            step,
        } => {
            rewrite_expr(object, subs);
            for b in [lower, upper, step].into_iter().flatten() {
                rewrite_expr(b, subs);
            }
        }
        ExprKind::ListLit(elems) | ExprKind::TupleLit(elems) => rewrite_exprs(elems, subs),
        ExprKind::TypeValue(_) => {}
        ExprKind::Invoke { .. } => {}
        ExprKind::BraceLit(_) => {}
        ExprKind::Named { value, .. } => rewrite_expr(value, subs),
        ExprKind::IfExpr {
            cond,
            then_branch,
            else_branch,
        } => {
            rewrite_expr(cond, subs);
            rewrite_expr(then_branch, subs);
            rewrite_expr(else_branch, subs);
        }
    }
}

fn rewrite_exprs(es: &mut [Expr], subs: Subs) {
    for e in es {
        rewrite_expr(e, subs);
    }
}

fn rewrite_param_args(args: &mut [crate::ast::ParamArg], subs: Subs) {
    for a in args {
        if let crate::ast::ParamArg::Value(e) = a {
            rewrite_expr(e, subs);
        }
    }
}

fn rewrite_block(body: &mut [Stmt], subs: Subs, into_defs: bool) {
    for s in body {
        rewrite_stmt(s, subs, into_defs);
    }
}

fn rewrite_stmt(s: &mut Stmt, subs: Subs, into_defs: bool) {
    match &mut s.kind {
        StmtKind::VarDecl { value, .. }
        | StmtKind::RefDecl { value, .. }
        | StmtKind::Assign { value, .. }
        | StmtKind::Comptime { value, .. }
        | StmtKind::Raise(value)
        | StmtKind::Return(Some(value)) => rewrite_expr(value, subs),
        StmtKind::Return(None) | StmtKind::Pass | StmtKind::Break | StmtKind::Continue => {}
        StmtKind::Import { .. } | StmtKind::FromImport { .. } => {}
        StmtKind::SetPlace { place, value } | StmtKind::AugAssign { place, value, .. } => {
            rewrite_expr(place, subs);
            rewrite_expr(value, subs);
        }
        StmtKind::Unpack { targets, value } => {
            rewrite_exprs(targets, subs);
            rewrite_expr(value, subs);
        }
        StmtKind::Expr(e) => rewrite_expr(e, subs),
        StmtKind::If { branches, orelse } | StmtKind::ComptimeIf { branches, orelse } => {
            for (c, b) in branches {
                rewrite_expr(c, subs);
                rewrite_block(b, subs, into_defs);
            }
            if let Some(b) = orelse {
                rewrite_block(b, subs, into_defs);
            }
        }
        StmtKind::While { cond, body } => {
            rewrite_expr(cond, subs);
            rewrite_block(body, subs, into_defs);
        }
        StmtKind::For { iter, body, .. } | StmtKind::ComptimeFor { iter, body, .. } => {
            rewrite_expr(iter, subs);
            rewrite_block(body, subs, into_defs);
        }
        StmtKind::Try {
            body,
            except,
            orelse,
            finalbody,
        } => {
            rewrite_block(body, subs, into_defs);
            if let Some((_, b)) = except {
                rewrite_block(b, subs, into_defs);
            }
            if let Some(b) = orelse {
                rewrite_block(b, subs, into_defs);
            }
            if let Some(b) = finalbody {
                rewrite_block(b, subs, into_defs);
            }
        }
        StmtKind::With { items, body } => {
            for WithItem { context, .. } in items {
                rewrite_expr(context, subs);
            }
            rewrite_block(body, subs, into_defs);
        }
        // A nested `def`/`struct` is a separate scope. For materialization
        // (`into_defs`), descend but shadow the function's parameters (a parameter
        // named like a module constant is *not* that constant). For loop-variable
        // substitution, don't descend (the loop var is an outer compile-time symbol).
        StmtKind::Def { params, body, .. } => {
            if into_defs {
                let shadowed: HashSet<&str> = params.iter().map(|p| p.name.as_str()).collect();
                let inner: Subs = &|n| if shadowed.contains(n) { None } else { subs(n) };
                rewrite_block(body, inner, into_defs);
            }
        }
        StmtKind::Struct { methods, .. } => {
            if into_defs {
                for m in methods {
                    let mut shadowed: HashSet<&str> =
                        m.params.iter().map(|p| p.name.as_str()).collect();
                    shadowed.insert("self");
                    let inner: Subs = &|n| if shadowed.contains(n) { None } else { subs(n) };
                    rewrite_block(&mut m.body, inner, into_defs);
                }
            }
        }
        StmtKind::Trait { .. } => {}
    }
}
