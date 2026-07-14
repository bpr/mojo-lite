use super::*;

// --- Nested `def` (closure) lifting -----------------------------------------

/// Collect the single-level nested `def` statements of a function body (those
/// directly in the body or in its control-flow blocks), without descending into
/// their own bodies.
fn find_nested_defs<'a>(body: &'a [Stmt], out: &mut Vec<&'a Stmt>) {
    for s in body {
        match &s.kind {
            StmtKind::Def { .. } => out.push(s),
            StmtKind::If { branches, orelse } => {
                for (_, b) in branches {
                    find_nested_defs(b, out);
                }
                if let Some(e) = orelse {
                    find_nested_defs(e, out);
                }
            }
            StmtKind::While { body, .. } | StmtKind::For { body, .. } => {
                find_nested_defs(body, out)
            }
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                find_nested_defs(body, out);
                if let Some((_, b)) = except {
                    find_nested_defs(b, out);
                }
                if let Some(e) = orelse {
                    find_nested_defs(e, out);
                }
                if let Some(f) = finalbody {
                    find_nested_defs(f, out);
                }
            }
            _ => {}
        }
    }
}

/// Collect the names a statement list *binds* in the enclosing (flat) frame: `var`
/// / `comptime` / `for` vars, `except` bindings, unpack targets, and nested `def`
/// names. Descends into control-flow blocks but not `def`/`struct`/`trait` bodies.
fn binds(body: &[Stmt], out: &mut HashSet<String>) {
    for s in body {
        match &s.kind {
            StmtKind::VarDecl { name, .. }
            | StmtKind::Comptime { name, .. }
            | StmtKind::Def { name, .. } => {
                out.insert(name.clone());
            }
            StmtKind::For { var, body, .. } => {
                out.insert(var.clone());
                binds(body, out);
            }
            StmtKind::If { branches, orelse } => {
                for (_, b) in branches {
                    binds(b, out);
                }
                if let Some(e) = orelse {
                    binds(e, out);
                }
            }
            StmtKind::While { body, .. } => binds(body, out),
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                binds(body, out);
                if let Some((n, b)) = except {
                    if let Some(n) = n {
                        out.insert(n.clone());
                    }
                    binds(b, out);
                }
                if let Some(e) = orelse {
                    binds(e, out);
                }
                if let Some(f) = finalbody {
                    binds(f, out);
                }
            }
            StmtKind::Unpack { targets, .. } => {
                for t in targets {
                    if let ExprKind::Identifier(n) = &t.kind {
                        out.insert(n.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

/// Collect every identifier *referenced* by an expression (reads, callee names,
/// receivers, indices, …).
fn refs_expr(e: &Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::Identifier(n) => {
            out.insert(n.clone());
        }
        ExprKind::Prefix(_, a) | ExprKind::Transfer(a) => refs_expr(a, out),
        ExprKind::Infix(_, a, b) => {
            refs_expr(a, out);
            refs_expr(b, out);
        }
        ExprKind::Call {
            name,
            param_args,
            args,
            kwargs,
        } => {
            out.insert(name.clone());
            for pa in param_args {
                if let ParamArg::Value(x) = pa {
                    refs_expr(x, out);
                }
            }
            for a in args {
                refs_expr(a, out);
            }
            for k in kwargs {
                refs_expr(&k.value, out);
            }
        }
        ExprKind::Member { object, .. } => refs_expr(object, out),
        ExprKind::MethodCall {
            object,
            args,
            kwargs,
            ..
        } => {
            refs_expr(object, out);
            for a in args {
                refs_expr(a, out);
            }
            for k in kwargs {
                refs_expr(&k.value, out);
            }
        }
        ExprKind::Index { object, index } => {
            refs_expr(object, out);
            refs_expr(index, out);
        }
        ExprKind::ListLit(es) | ExprKind::TupleLit(es) => {
            for x in es {
                refs_expr(x, out);
            }
        }
        ExprKind::Named { value, .. } => refs_expr(value, out),
        ExprKind::IfExpr {
            cond,
            then_branch,
            else_branch,
        } => {
            refs_expr(cond, out);
            refs_expr(then_branch, out);
            refs_expr(else_branch, out);
        }
        ExprKind::Compare { first, rest } => {
            refs_expr(first, out);
            for (_, x) in rest {
                refs_expr(x, out);
            }
        }
        ExprKind::Slice {
            object,
            lower,
            upper,
            step,
        } => {
            refs_expr(object, out);
            for x in [lower, upper, step].into_iter().flatten() {
                refs_expr(x, out);
            }
        }
        ExprKind::TString { parts, .. } => {
            for p in parts {
                if let TStringPart::Expr(x) = p {
                    refs_expr(x, out);
                }
            }
        }
        _ => {} // literals, None
    }
}

/// Collect identifiers referenced by a statement list; returns `false` if the body
/// contains a nested `def`/`struct`/`trait` (can't lift — deeper nesting is
/// refused). Does not descend into such nested bodies.
fn refs_stmts(body: &[Stmt], out: &mut HashSet<String>) -> bool {
    let mut ok = true;
    for s in body {
        match &s.kind {
            StmtKind::Def { .. } | StmtKind::Struct { .. } | StmtKind::Trait { .. } => ok = false,
            StmtKind::VarDecl { value, .. } | StmtKind::Comptime { value, .. } => {
                refs_expr(value, out)
            }
            StmtKind::Assign { name, value } => {
                out.insert(name.clone());
                refs_expr(value, out);
            }
            StmtKind::AugAssign { place, value, .. } => {
                refs_expr(place, out);
                refs_expr(value, out);
            }
            StmtKind::SetPlace { place, value } => {
                refs_expr(place, out);
                refs_expr(value, out);
            }
            StmtKind::If { branches, orelse } => {
                for (c, b) in branches {
                    refs_expr(c, out);
                    ok &= refs_stmts(b, out);
                }
                if let Some(e) = orelse {
                    ok &= refs_stmts(e, out);
                }
            }
            StmtKind::While { cond, body } => {
                refs_expr(cond, out);
                ok &= refs_stmts(body, out);
            }
            StmtKind::For { iter, body, .. } => {
                refs_expr(iter, out);
                ok &= refs_stmts(body, out);
            }
            StmtKind::Return(Some(e)) | StmtKind::Raise(e) | StmtKind::Expr(e) => refs_expr(e, out),
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                ok &= refs_stmts(body, out);
                if let Some((_, b)) = except {
                    ok &= refs_stmts(b, out);
                }
                if let Some(e) = orelse {
                    ok &= refs_stmts(e, out);
                }
                if let Some(f) = finalbody {
                    ok &= refs_stmts(f, out);
                }
            }
            StmtKind::Unpack { targets, value } => {
                for t in targets {
                    refs_expr(t, out);
                }
                refs_expr(value, out);
            }
            _ => {}
        }
    }
    ok
}

/// Compute a nested `def`'s captures (the enclosing-frame locals it references),
/// or `None` if it can't be lifted: it declares its own nested `def`/`struct`/
/// `trait`, or it calls a *sibling* nested `def` (whose captures we can't forward).
/// A self-reference is fine (self-recursion via the registry, not a capture).
fn analyze_captures(
    dparams: &[FnParam],
    dbody: &[Stmt],
    f_bound: &HashSet<String>,
    nested_names: &HashSet<String>,
    self_name: &str,
) -> Option<Vec<String>> {
    let mut d_bound: HashSet<String> = dparams.iter().map(|p| p.name.clone()).collect();
    binds(dbody, &mut d_bound);
    let mut used = HashSet::new();
    if !refs_stmts(dbody, &mut used) {
        return None; // contains a deeper nested declaration
    }
    let mut captures: Vec<String> = used
        .into_iter()
        .filter(|n| !d_bound.contains(n) && f_bound.contains(n))
        .collect();
    if captures
        .iter()
        .any(|n| nested_names.contains(n) && n != self_name)
    {
        return None; // references a sibling nested `def`
    }
    captures.retain(|n| !nested_names.contains(n)); // drop a self-reference
    captures.sort();
    Some(captures)
}

/// Lower a function body (`name` its registered/mangled name) plus every nested
/// `def` it defines, pushing the function and each lifted nested function into
/// `out`. A liftable nested `def` becomes `name$inner` with its captured enclosing
/// locals as leading `mut` parameters (pass-through-typed so `coerce` is identity);
/// a nested `def` we can't lift stays a clean `Unsupported` at execution.
pub(super) struct FunctionLowering<'a> {
    pub(super) name: &'a str,
    pub(super) parameter_names: &'a [String],
    pub(super) parameter_annotations: Vec<SourceType>,
    pub(super) owned_parameters: Vec<bool>,
    pub(super) reference_parameters: Vec<bool>,
    pub(super) returns_reference: bool,
    pub(super) body: &'a [Stmt],
    pub(super) overloads: &'a crate::symbol::OverloadSets,
    pub(super) overload_targets: &'a HashMap<SourceSpan, String>,
}

pub(super) fn lower_fn_nested(request: FunctionLowering<'_>, out: &mut Vec<(String, MirFunction)>) {
    let FunctionLowering {
        name,
        parameter_names: param_names,
        parameter_annotations: param_annotations,
        owned_parameters: owned_params,
        reference_parameters: ref_params,
        returns_reference,
        body,
        overloads,
        overload_targets,
    } = request;
    let mut f_bound: HashSet<String> = param_names.iter().cloned().collect();
    binds(body, &mut f_bound);

    let mut nested_defs = Vec::new();
    find_nested_defs(body, &mut nested_defs);
    let nested_names: HashSet<String> = nested_defs
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::Def { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();

    let mut registry: HashMap<String, NestedInfo> = HashMap::new();
    let mut liftable: Vec<(&Stmt, Vec<String>, String)> = Vec::new();
    for ds in &nested_defs {
        if let StmtKind::Def {
            name: dname,
            type_params,
            params: dparams,
            body: dbody,
            ..
        } = &ds.kind
        {
            if !type_params.is_empty() {
                continue; // a generic nested `def` is refused (stays Unsupported)
            }
            if let Some(captures) = analyze_captures(dparams, dbody, &f_bound, &nested_names, dname)
            {
                let mangled = crate::symbol::nested_lifted_name(name, dname);
                registry.insert(
                    dname.clone(),
                    NestedInfo {
                        mangled: mangled.clone(),
                        captures: captures.clone(),
                    },
                );
                liftable.push((ds, captures, mangled));
            }
        }
    }

    let cfg = Cfg::build_fn(param_names, body);
    let mut f = lower_cfg_nested(
        &cfg,
        &registry,
        overloads,
        overload_targets,
        returns_reference,
    );
    f.param_annotations = param_annotations;
    f.owned_params = owned_params;
    f.ref_params = ref_params;
    f.returns_reference = returns_reference;
    out.push((name.to_string(), f));

    let cap_ty = SourceType::Named("$capture".to_string(), Vec::new());
    for (ds, captures, mangled) in liftable {
        if let StmtKind::Def {
            params: dparams,
            body: dbody,
            ..
        } = &ds.kind
        {
            let mut names: Vec<String> = captures.clone();
            names.extend(dparams.iter().map(|p| p.name.clone()));
            let mut ptys: Vec<SourceType> = vec![cap_ty.clone(); captures.len()];
            ptys.extend(dparams.iter().map(|p| p.ty.clone()));
            let mut owned2 = vec![false; captures.len()];
            owned2.extend(dparams.iter().map(|p| is_owned(&p.convention)));
            // Captures are `mut` (their final value is written back to the enclosing
            // variable — reference-capture semantics).
            let mut refp2 = vec![true; captures.len()];
            refp2.extend(dparams.iter().map(|p| is_ref(&p.convention)));
            let immutable_captures: HashSet<String> = captures
                .iter()
                .filter(|capture| {
                    body.iter().any(|stmt| {
                        matches!(
                            &stmt.kind,
                            StmtKind::Comptime { name, .. } if name == *capture
                        )
                    })
                })
                .cloned()
                .collect();
            let ncfg = Cfg::build_fn_with_captures(&names, immutable_captures, dbody);
            let mut nf = lower_cfg_nested(&ncfg, &registry, overloads, overload_targets, false);
            nf.param_annotations = ptys;
            nf.owned_params = owned2;
            nf.ref_params = refp2;
            out.push((mangled, nf));
        }
    }
}
