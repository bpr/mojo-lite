//! AST rewriting helpers used by compile-time elaboration and specialization.

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

/// Replace a specialized type-pack use such as `Tuple[*Ts]` with the concrete
/// parameter list selected for this specialization.  The parser deliberately
/// retains `*Ts` as a source type so expansion remains an explicit compile-time
/// operation rather than something the checker has to reconstruct from names.
pub(super) fn expand_type_packs(ty: &mut Type, packs: &HashMap<String, Vec<Type>>) {
    match ty {
        Type::Named(_, arguments) => {
            expand_type_pack_arguments(arguments, packs);
        }
        Type::Assoc { base, .. } => expand_type_packs(base, packs),
        Type::Func {
            params,
            ret,
            raises_type,
            ..
        } => {
            for param in params {
                expand_type_packs(param, packs);
            }
            expand_type_packs(ret, packs);
            if let Some(error) = raises_type {
                expand_type_packs(error, packs);
            }
        }
        Type::Ref { referent, .. } => expand_type_packs(referent, packs),
        Type::Int
        | Type::UInt
        | Type::Bool
        | Type::String
        | Type::Float64
        | Type::None
        | Type::SelfParam(_)
        | Type::SelfType => {}
    }
}

fn expand_type_pack_arguments(arguments: &mut Vec<ParamArg>, packs: &HashMap<String, Vec<Type>>) {
    let mut expanded = Vec::with_capacity(arguments.len());
    for mut argument in std::mem::take(arguments) {
        match &mut argument {
            ParamArg::Type(Type::Named(name, nested))
                if name.starts_with('*') && nested.is_empty() =>
            {
                if let Some(types) = packs.get(name.trim_start_matches('*')) {
                    expanded.extend(types.iter().cloned().map(ParamArg::Type));
                } else {
                    expanded.push(argument);
                }
            }
            ParamArg::Type(ty) => {
                expand_type_packs(ty, packs);
                expanded.push(argument);
            }
            ParamArg::Named { value, .. } => {
                expand_type_pack_argument(value, packs);
                expanded.push(argument);
            }
            ParamArg::Value(_) => expanded.push(argument),
        }
    }
    *arguments = expanded;
}

fn expand_type_pack_argument(argument: &mut ParamArg, packs: &HashMap<String, Vec<Type>>) {
    match argument {
        ParamArg::Type(ty) => expand_type_packs(ty, packs),
        ParamArg::Named { value, .. } => expand_type_pack_argument(value, packs),
        ParamArg::Value(_) => {}
    }
}

/// Expand `*args^` in a specialized `Tuple[*Ts](...)` construction.  Current
/// Mojo intentionally does not permit arbitrary pack expansion into a
/// fixed-arity call, so this rewrite is scoped to Tuple construction.  Other
/// spreads survive to the checker and receive the normal unsupported diagnostic.
pub(super) fn expand_pack_spreads_in_block(
    statements: &mut [Stmt],
    type_packs: &HashMap<String, Vec<Type>>,
    runtime_pack_lengths: &HashMap<String, usize>,
) {
    for statement in statements {
        expand_pack_spreads_in_stmt(statement, type_packs, runtime_pack_lengths);
    }
}

fn expand_pack_spreads_in_stmt(
    statement: &mut Stmt,
    type_packs: &HashMap<String, Vec<Type>>,
    runtime_pack_lengths: &HashMap<String, usize>,
) {
    let expr =
        |value: &mut Expr| expand_pack_spreads_in_expr(value, type_packs, runtime_pack_lengths);
    match &mut statement.kind {
        StmtKind::VarDecl { ty, value, .. } => {
            if let Some(ty) = ty {
                expand_type_packs(ty, type_packs);
            }
            expr(value);
        }
        StmtKind::RefDecl { value, .. }
        | StmtKind::Assign { value, .. }
        | StmtKind::Comptime { value, .. }
        | StmtKind::Raise(value)
        | StmtKind::Return(Some(value))
        | StmtKind::Expr(value) => expr(value),
        StmtKind::SetPlace { place, value } | StmtKind::AugAssign { place, value, .. } => {
            expr(place);
            expr(value);
        }
        StmtKind::Unpack { targets, value } => {
            for target in targets {
                expr(target);
            }
            expr(value);
        }
        StmtKind::If { branches, orelse } | StmtKind::ComptimeIf { branches, orelse } => {
            for (condition, body) in branches {
                expr(condition);
                expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
            }
            if let Some(body) = orelse {
                expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
            }
        }
        StmtKind::While { cond, body, orelse } => {
            expr(cond);
            expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
            if let Some(body) = orelse {
                expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
            }
        }
        StmtKind::For {
            iter, body, orelse, ..
        } => {
            expr(iter);
            expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
            if let Some(body) = orelse {
                expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
            }
        }
        StmtKind::ComptimeFor { iter, body, .. } => {
            expr(iter);
            expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
        }
        StmtKind::Try {
            body,
            except,
            orelse,
            finalbody,
        } => {
            expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
            if let Some((_, body)) = except {
                expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
            }
            if let Some(body) = orelse {
                expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
            }
            if let Some(body) = finalbody {
                expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
            }
        }
        StmtKind::With { items, body } => {
            for item in items {
                expr(&mut item.context);
            }
            expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
        }
        StmtKind::Def {
            params,
            raises_type,
            ret,
            body,
            ..
        } => {
            for parameter in params {
                expand_type_packs(&mut parameter.ty, type_packs);
                if let Some(default) = &mut parameter.default {
                    expr(default);
                }
            }
            if let Some(error) = raises_type {
                expand_type_packs(error, type_packs);
            }
            if let Some(ret) = ret {
                expand_type_packs(ret, type_packs);
            }
            expand_pack_spreads_in_block(body, type_packs, runtime_pack_lengths);
        }
        StmtKind::Struct {
            fields,
            associated,
            methods,
            ..
        } => {
            for field in fields {
                expand_type_packs(&mut field.ty, type_packs);
            }
            for item in associated {
                expr(&mut item.value);
            }
            for method in methods {
                for parameter in &mut method.params {
                    expand_type_packs(&mut parameter.ty, type_packs);
                    if let Some(default) = &mut parameter.default {
                        expr(default);
                    }
                }
                if let Some(error) = &mut method.raises_type {
                    expand_type_packs(error, type_packs);
                }
                if let Some(ret) = &mut method.ret {
                    expand_type_packs(ret, type_packs);
                }
                expand_pack_spreads_in_block(&mut method.body, type_packs, runtime_pack_lengths);
            }
        }
        StmtKind::Return(None)
        | StmtKind::Pass
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Import { .. }
        | StmtKind::FromImport { .. }
        | StmtKind::Trait { .. } => {}
    }
}

fn expand_pack_spreads_in_expr(
    expression: &mut Expr,
    type_packs: &HashMap<String, Vec<Type>>,
    runtime_pack_lengths: &HashMap<String, usize>,
) {
    match &mut expression.kind {
        ExprKind::Call {
            name,
            param_args,
            args,
            kwargs,
        } => {
            expand_type_pack_arguments(param_args, type_packs);
            for argument in args.iter_mut() {
                expand_pack_spreads_in_expr(argument, type_packs, runtime_pack_lengths);
            }
            for argument in kwargs {
                expand_pack_spreads_in_expr(&mut argument.value, type_packs, runtime_pack_lengths);
            }
            if name == "Tuple" {
                *args = expand_tuple_spread_arguments(std::mem::take(args), runtime_pack_lengths);
            }
        }
        ExprKind::Invoke {
            callee,
            param_args,
            args,
            kwargs,
        } => {
            expand_pack_spreads_in_expr(callee, type_packs, runtime_pack_lengths);
            expand_type_pack_arguments(param_args, type_packs);
            for argument in args {
                expand_pack_spreads_in_expr(argument, type_packs, runtime_pack_lengths);
            }
            for argument in kwargs {
                expand_pack_spreads_in_expr(&mut argument.value, type_packs, runtime_pack_lengths);
            }
        }
        ExprKind::TypeApply { args, .. } => {
            expand_type_pack_arguments(args, type_packs);
        }
        ExprKind::Prefix(_, value) | ExprKind::Transfer(value) | ExprKind::Spread(value) => {
            expand_pack_spreads_in_expr(value, type_packs, runtime_pack_lengths)
        }
        ExprKind::Infix(_, left, right)
        | ExprKind::Index {
            object: left,
            index: right,
        } => {
            expand_pack_spreads_in_expr(left, type_packs, runtime_pack_lengths);
            expand_pack_spreads_in_expr(right, type_packs, runtime_pack_lengths);
        }
        ExprKind::Compare { first, rest } => {
            expand_pack_spreads_in_expr(first, type_packs, runtime_pack_lengths);
            for (_, value) in rest {
                expand_pack_spreads_in_expr(value, type_packs, runtime_pack_lengths);
            }
        }
        ExprKind::Member { object, .. } => {
            expand_pack_spreads_in_expr(object, type_packs, runtime_pack_lengths)
        }
        ExprKind::MethodCall {
            object,
            args,
            kwargs,
            ..
        } => {
            expand_pack_spreads_in_expr(object, type_packs, runtime_pack_lengths);
            for argument in args {
                expand_pack_spreads_in_expr(argument, type_packs, runtime_pack_lengths);
            }
            for argument in kwargs {
                expand_pack_spreads_in_expr(&mut argument.value, type_packs, runtime_pack_lengths);
            }
        }
        ExprKind::Slice {
            object,
            lower,
            upper,
            step,
            ..
        } => {
            expand_pack_spreads_in_expr(object, type_packs, runtime_pack_lengths);
            for bound in [lower, upper, step].into_iter().flatten() {
                expand_pack_spreads_in_expr(bound, type_packs, runtime_pack_lengths);
            }
        }
        ExprKind::MultiIndex { object, args } => {
            expand_pack_spreads_in_expr(object, type_packs, runtime_pack_lengths);
            for argument in args {
                match argument {
                    crate::ast::SubscriptArg::Index(value) => {
                        expand_pack_spreads_in_expr(value, type_packs, runtime_pack_lengths)
                    }
                    crate::ast::SubscriptArg::Slice {
                        lower, upper, step, ..
                    } => {
                        for value in [lower, upper, step].into_iter().flatten() {
                            expand_pack_spreads_in_expr(value, type_packs, runtime_pack_lengths);
                        }
                    }
                }
            }
        }
        ExprKind::ListLit(values) | ExprKind::TupleLit(values) => {
            for value in values {
                expand_pack_spreads_in_expr(value, type_packs, runtime_pack_lengths);
            }
        }
        ExprKind::BraceLit(entries) => {
            for (key, value) in entries {
                expand_pack_spreads_in_expr(key, type_packs, runtime_pack_lengths);
                if let Some(value) = value {
                    expand_pack_spreads_in_expr(value, type_packs, runtime_pack_lengths);
                }
            }
        }
        ExprKind::Comprehension {
            key,
            value,
            clauses,
            ..
        } => {
            for clause in clauses {
                match clause {
                    crate::ast::ComprehensionClause::For { iter, .. } => {
                        expand_pack_spreads_in_expr(iter, type_packs, runtime_pack_lengths)
                    }
                    crate::ast::ComprehensionClause::If(condition) => {
                        expand_pack_spreads_in_expr(condition, type_packs, runtime_pack_lengths)
                    }
                }
            }
            if let Some(key) = key {
                expand_pack_spreads_in_expr(key, type_packs, runtime_pack_lengths);
            }
            expand_pack_spreads_in_expr(value, type_packs, runtime_pack_lengths);
        }
        ExprKind::TypeValue(ty) => expand_type_packs(ty, type_packs),
        ExprKind::Named { value, .. } => {
            expand_pack_spreads_in_expr(value, type_packs, runtime_pack_lengths)
        }
        ExprKind::IfExpr {
            cond,
            then_branch,
            else_branch,
        } => {
            expand_pack_spreads_in_expr(cond, type_packs, runtime_pack_lengths);
            expand_pack_spreads_in_expr(then_branch, type_packs, runtime_pack_lengths);
            expand_pack_spreads_in_expr(else_branch, type_packs, runtime_pack_lengths);
        }
        ExprKind::TString { parts, .. } => {
            for part in parts {
                if let crate::ast::TStringPart::Expr(value) = part {
                    expand_pack_spreads_in_expr(value, type_packs, runtime_pack_lengths);
                }
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::None
        | ExprKind::Uninitialized
        | ExprKind::Identifier(_) => {}
    }
}

fn expand_tuple_spread_arguments(
    arguments: Vec<Expr>,
    runtime_pack_lengths: &HashMap<String, usize>,
) -> Vec<Expr> {
    let mut expanded = Vec::new();
    for argument in arguments {
        let ExprKind::Spread(value) = &argument.kind else {
            expanded.push(argument);
            continue;
        };
        let (pack, transferring) = match &value.kind {
            ExprKind::Identifier(name) => (name.as_str(), false),
            ExprKind::Transfer(value) => match &value.kind {
                ExprKind::Identifier(name) => (name.as_str(), true),
                _ => {
                    expanded.push(argument);
                    continue;
                }
            },
            _ => {
                expanded.push(argument);
                continue;
            }
        };
        let Some(length) = runtime_pack_lengths.get(pack) else {
            expanded.push(argument);
            continue;
        };
        let source = argument.source.clone();
        let span = argument.span;
        for index in 0..*length {
            let base = Expr {
                kind: ExprKind::Identifier(pack.to_string()),
                span,
                source: source.clone(),
            };
            let index = Expr {
                kind: ExprKind::Int(index as i64),
                span,
                source: source.clone(),
            };
            let indexed = Expr {
                kind: ExprKind::Index {
                    object: Box::new(base),
                    index: Box::new(index),
                },
                span,
                source: source.clone(),
            };
            expanded.push(if transferring {
                Expr {
                    kind: ExprKind::Transfer(Box::new(indexed)),
                    span,
                    source: source.clone(),
                }
            } else {
                indexed
            });
        }
    }
    expanded
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
        ExprKind::Spread(value) => rewrite_expr(value, subs),
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
            ..
        } => {
            rewrite_expr(object, subs);
            for b in [lower, upper, step].into_iter().flatten() {
                rewrite_expr(b, subs);
            }
        }
        ExprKind::MultiIndex { object, args } => {
            rewrite_expr(object, subs);
            for argument in args {
                match argument {
                    crate::ast::SubscriptArg::Index(value) => rewrite_expr(value, subs),
                    crate::ast::SubscriptArg::Slice {
                        lower, upper, step, ..
                    } => {
                        for value in [lower, upper, step].into_iter().flatten() {
                            rewrite_expr(value, subs);
                        }
                    }
                }
            }
        }
        ExprKind::ListLit(elems) | ExprKind::TupleLit(elems) => rewrite_exprs(elems, subs),
        ExprKind::TypeValue(_) => {}
        ExprKind::Invoke { .. } => {}
        ExprKind::BraceLit(_) => {}
        // Comprehension targets introduce a nested lexical environment. The
        // runtime checker/lowerer handles their expressions; blindly applying
        // this flat comptime substitution could replace a shadowed target.
        ExprKind::Comprehension { .. } => {}
        ExprKind::Uninitialized => {}
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
        StmtKind::While { cond, body, orelse } => {
            rewrite_expr(cond, subs);
            rewrite_block(body, subs, into_defs);
            if let Some(body) = orelse {
                rewrite_block(body, subs, into_defs);
            }
        }
        StmtKind::For {
            iter, body, orelse, ..
        } => {
            rewrite_expr(iter, subs);
            rewrite_block(body, subs, into_defs);
            if let Some(body) = orelse {
                rewrite_block(body, subs, into_defs);
            }
        }
        StmtKind::ComptimeFor { iter, body, .. } => {
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
