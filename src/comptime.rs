//! Compile-time elaboration (Phase 4 — comptime semantics).
//!
//! A pass between parsing and type-checking that **resolves compile-time
//! constructs before runtime lowering**, per `docs/notes/comptime.md`:
//! `comptime` is a *phase distinction*, so the elaborator rewrites the AST so the
//! checker/MIR/VM only ever see ordinary code.
//!
//! - **`comptime NAME = expr`** — evaluated at compile time (a compile-time value is
//!   required; the elaborator is the validator). Recorded in a compile-time
//!   environment; the statement is kept as an ordinary binding.
//! - **`comptime if`** — keeps only the taken branch; the others are dropped before
//!   type-checking.
//! - **`comptime for`** — unrolls over a compile-time `range(...)` or a compile-time
//!   tuple/list, substituting the loop variable with its literal in each body copy;
//!   a **fuel quota** bounds the work.
//! - **CTFE** — a `comptime` context may call a **pure top-level function**. The
//!   elaborator verifies a restricted helper call graph, folds compile-time-only
//!   facts such as `T.size` and `is_same_type[T, U]()` into literals, and executes
//!   the resulting helper through HIR/MIR on the register VM with a shared fuel
//!   budget. This keeps function-body execution on the same path as runtime code.
//! - **Materialization** — module-level `comptime` constants are inlined as literals
//!   into runtime code, so a top-level comptime value is usable inside functions.
//! - **Delayed generic elaboration (Phase 6)** — a generic `def` whose (value)
//!   parameters feed a `comptime if`/`comptime for` cannot be elaborated early (the
//!   parameter value is only known per call). Such a def is kept as a *template*;
//!   a monomorphization pass then specializes it per distinct value argument,
//!   resolving the comptime construct so only the *selected* branch is type-checked
//!   (`f[0]` and `f[1]` take different branches, and a type error in a dropped
//!   branch is never seen).
//!
//! Compile-time values are the shared [`CtValue`](crate::ct::CtValue) universe:
//! runtime-materializable `Int`/`Bool`/`String`/`Tuple`/`List`, plus
//! compile-time-only `Type` and symbolic `Param` facts.

use crate::ast::{
    Expr, ExprKind, InfixOp, ParamArg, PrefixOp, Stmt, StmtKind, StructComptime, Type, TypeParam,
    WithItem,
};
use crate::backend::VmBackend;
use crate::ct::CtValue;
use crate::runtime::Value;
use crate::token::Span;
use crate::types::{ParamDecl, Ty, TyArg};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};

/// The maximum number of compile-time "steps" (loop iterations, statements
/// executed, function calls) across a whole program — a hard bound so compile-time
/// execution can't hang the compiler (cf. Zig's quota).
const FUEL: usize = 100_000;

/// Comptime-specific accessors on the shared [`CtValue`], reporting a
/// [`ComptimeError`] when a value is not of the required kind.
impl CtValue {
    fn as_bool(&self, ctx: &str) -> Result<bool, ComptimeError> {
        match self {
            CtValue::Bool(b) => Ok(*b),
            _ => Err(ComptimeError::NotBool(ctx.to_string())),
        }
    }
    fn as_int(&self, ctx: &str) -> Result<i64, ComptimeError> {
        match self {
            CtValue::Int(n) => Ok(*n),
            _ => Err(ComptimeError::NotInt(ctx.to_string())),
        }
    }
    /// The elements of a compile-time collection (`Tuple`/`List`), for iteration.
    fn as_sequence(&self, ctx: &str) -> Result<Vec<CtValue>, ComptimeError> {
        match self {
            CtValue::Tuple(v) | CtValue::List(v) => Ok(v.clone()),
            _ => Err(ComptimeError::BadRange(ctx.to_string())),
        }
    }
}

/// An error from compile-time elaboration.
#[derive(Debug)]
pub enum ComptimeError {
    /// An expression is not compile-time evaluable (or names an unknown comptime).
    NotComptime(String),
    /// A condition did not evaluate to `Bool`.
    NotBool(String),
    /// A context required a compile-time `Int`.
    NotInt(String),
    /// Integer `//`/`%` by zero, or a negative `**` exponent, at compile time.
    BadArithmetic(String),
    /// A `comptime for` iterable was not a `range(...)` / tuple / list.
    BadRange(String),
    /// A CTFE call had the wrong number of arguments.
    Arity(String),
    /// The compile-time step/iteration quota was exceeded (a likely infinite loop).
    QuotaExceeded,
}

impl std::fmt::Display for ComptimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComptimeError::NotComptime(s) => write!(f, "not a compile-time value: {s}"),
            ComptimeError::NotBool(s) => write!(f, "expected a compile-time Bool ({s})"),
            ComptimeError::NotInt(s) => write!(f, "expected a compile-time Int ({s})"),
            ComptimeError::BadArithmetic(s) => write!(f, "compile-time arithmetic error: {s}"),
            ComptimeError::BadRange(s) => {
                write!(f, "'comptime for' needs a range(...)/tuple/list: {s}")
            }
            ComptimeError::Arity(s) => write!(f, "compile-time call arity: {s}"),
            ComptimeError::QuotaExceeded => {
                write!(f, "compile-time execution exceeded the step quota ({FUEL})")
            }
        }
    }
}

/// A CTFE-callable function: a pure top-level `def`, optionally with compile-time
/// parameters specialized at the call site.
struct CtFn<'a> {
    ct_params: Vec<ParamDecl>,
    params: Vec<String>,
    body: &'a [Stmt],
}

/// Compile-time metadata for a top-level struct, enough for generic CTFE to read
/// associated facts such as `T.size`.
struct CtStruct<'a> {
    decls: Vec<ParamDecl>,
    associated: &'a [StructComptime],
}

/// The compile-time elaboration engine: the CTFE-callable functions and a shared
/// fuel budget. `top_consts` captures module-level constants for materialization;
/// `specializable` holds the comptime-dependent generic `def` templates (Phase 6).
struct Elab<'a> {
    program: &'a [Stmt],
    fns: HashMap<String, CtFn<'a>>,
    structs: HashMap<String, CtStruct<'a>>,
    /// Top-level generic `def`s whose value parameters feed a `comptime if`/`for`
    /// (so they must be monomorphized per call), by name → the template `Stmt`.
    specializable: HashMap<String, &'a Stmt>,
    fuel: Cell<usize>,
    top_consts: RefCell<HashMap<String, CtValue>>,
}

/// Elaborate all compile-time constructs in a program, returning an ordinary AST.
pub fn elaborate(program: Vec<Stmt>) -> Result<Vec<Stmt>, ComptimeError> {
    let elab = Elab {
        program: &program,
        fns: collect_fns(&program),
        structs: collect_structs(&program),
        specializable: collect_specializable(&program),
        fuel: Cell::new(FUEL),
        top_consts: RefCell::new(HashMap::new()),
    };
    let mut env = HashMap::new();
    let elaborated = elab.block(&program, &mut env, false)?;
    // Materialize module-level comptime constants into runtime literals.
    let consts = elab.top_consts.borrow().clone();
    let materialized = materialize_block(elaborated, &consts);
    // Monomorphize comptime-dependent generic templates against their call sites.
    let mut result = elab.monomorphize(materialized)?;
    for statement in &mut result {
        if let Some(source) = statement.module.clone() {
            crate::ast::stamp_source(std::slice::from_mut(statement), &source);
        }
    }
    Ok(result)
}

fn collect_fns(program: &[Stmt]) -> HashMap<String, CtFn<'_>> {
    let mut fns = HashMap::new();
    for s in program {
        if let StmtKind::Def {
            name,
            params,
            body,
            type_params,
            ..
        } = &s.kind
        {
            fns.insert(
                name.clone(),
                CtFn {
                    ct_params: classify_ct_params(type_params),
                    params: params.iter().map(|p| p.name.clone()).collect(),
                    body,
                },
            );
        }
    }
    fns
}

fn collect_structs(program: &[Stmt]) -> HashMap<String, CtStruct<'_>> {
    let mut structs = HashMap::new();
    for s in program {
        if let StmtKind::Struct {
            name,
            type_params,
            associated,
            ..
        } = &s.kind
        {
            structs.insert(
                name.clone(),
                CtStruct {
                    decls: classify_ct_params(type_params),
                    associated,
                },
            );
        }
    }
    structs
}

/// Collect the top-level generic `def`s that must be monomorphized (Phase 6/7): a
/// generic `def` (type and/or value parameters) whose body contains a
/// `comptime if`/`comptime for`. Such a construct may depend on the parameters
/// (e.g. `comptime if is_same_type[T, Int]()`), so it can only be resolved per call
/// site — each specialization binds the concrete arguments and resolves the
/// comptime construct, so only the *selected* branch is type-checked. Because the
/// elaborator does not infer types, such a `def` must be called with explicit
/// `[...]` arguments.
fn collect_specializable(program: &[Stmt]) -> HashMap<String, &Stmt> {
    let mut m = HashMap::new();
    for s in program {
        if let StmtKind::Def {
            name,
            type_params,
            body,
            ..
        } = &s.kind
            && !type_params.is_empty()
            && block_has_comptime(body)
        {
            m.insert(name.clone(), s);
        }
    }
    m
}

/// Whether a block directly contains a `comptime if`/`comptime for` (not descending
/// into nested `def`/`struct`, which have their own compile-time scope).
fn block_has_comptime(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_has_comptime)
}

fn stmt_has_comptime(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::ComptimeIf { .. } | StmtKind::ComptimeFor { .. } => true,
        StmtKind::If { branches, orelse } => {
            branches.iter().any(|(_, b)| block_has_comptime(b))
                || orelse.as_ref().is_some_and(|b| block_has_comptime(b))
        }
        StmtKind::While { body, .. } | StmtKind::For { body, .. } => block_has_comptime(body),
        StmtKind::With { body, .. } => block_has_comptime(body),
        StmtKind::Try {
            body,
            except,
            orelse,
            finalbody,
        } => {
            block_has_comptime(body)
                || except.as_ref().is_some_and(|(_, b)| block_has_comptime(b))
                || orelse.as_ref().is_some_and(|b| block_has_comptime(b))
                || finalbody.as_ref().is_some_and(|b| block_has_comptime(b))
        }
        _ => false,
    }
}

fn classify_ct_params(tps: &[TypeParam]) -> Vec<ParamDecl> {
    tps.iter()
        .map(|tp| {
            if let [only] = tp.bounds.as_slice()
                && only == "Int"
            {
                return ParamDecl::Value {
                    name: tp.name.clone(),
                };
            }
            ParamDecl::Type {
                name: tp.name.clone(),
                bounds: tp.bounds.clone(),
            }
        })
        .collect()
}

impl<'a> Elab<'a> {
    fn burn(&self) -> Result<(), ComptimeError> {
        let f = self
            .fuel
            .get()
            .checked_sub(1)
            .ok_or(ComptimeError::QuotaExceeded)?;
        self.fuel.set(f);
        Ok(())
    }

    /// Elaborate a block, resolving `comptime` constructs. `in_fn` is true inside a
    /// function/method body (so a comptime constant there is *not* module-level).
    fn block(
        &self,
        stmts: &[Stmt],
        env: &mut HashMap<String, CtValue>,
        in_fn: bool,
    ) -> Result<Vec<Stmt>, ComptimeError> {
        let mut out = Vec::new();
        for stmt in stmts {
            let first_new = out.len();
            self.stmt(stmt, env, in_fn, &mut out)?;
            if let Some(source) = stmt.module.as_deref() {
                crate::ast::stamp_source(&mut out[first_new..], source);
            }
        }
        Ok(out)
    }

    fn stmt(
        &self,
        stmt: &Stmt,
        env: &mut HashMap<String, CtValue>,
        in_fn: bool,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ComptimeError> {
        let span = stmt.span;
        match &stmt.kind {
            StmtKind::Comptime { name, value } => {
                let v = self.eval(value, env)?;
                if !in_fn {
                    self.top_consts.borrow_mut().insert(name.clone(), v.clone());
                }
                // Fold the definition to its literal value, so the checker and
                // runtime see a constant (and a CTFE-computed `Int`, which the
                // checker's own folder can't evaluate, becomes usable as a value
                // parameter and materializes cleanly).
                let folded = mk(
                    StmtKind::Comptime {
                        name: name.clone(),
                        value: lit_result(&v, span)?,
                    },
                    span,
                );
                env.insert(name.clone(), v);
                out.push(folded);
            }
            StmtKind::ComptimeIf { branches, orelse } => {
                for (cond, body) in branches {
                    if self.eval(cond, env)?.as_bool("comptime if condition")? {
                        out.extend(self.block(body, env, in_fn)?);
                        return Ok(());
                    }
                }
                if let Some(body) = orelse {
                    out.extend(self.block(body, env, in_fn)?);
                }
            }
            StmtKind::ComptimeFor { var, iter, body } => {
                for v in self.eval_iter(iter, env)? {
                    self.burn()?;
                    let subs: Subs = &|n| (n == var).then(|| v.clone());
                    let substituted: Vec<Stmt> = body
                        .iter()
                        .map(|s| rewrite_stmt_cloned(s, subs, false))
                        .collect();
                    out.extend(self.block(&substituted, env, in_fn)?);
                }
            }
            StmtKind::If { branches, orelse } => {
                let branches = branches
                    .iter()
                    .map(|(c, b)| Ok((c.clone(), self.block(b, env, in_fn)?)))
                    .collect::<Result<Vec<_>, ComptimeError>>()?;
                let orelse = self.opt_block(orelse, env, in_fn)?;
                out.push(mk(StmtKind::If { branches, orelse }, span));
            }
            StmtKind::While { cond, body } => {
                let body = self.block(body, env, in_fn)?;
                out.push(mk(
                    StmtKind::While {
                        cond: cond.clone(),
                        body,
                    },
                    span,
                ));
            }
            StmtKind::For { var, iter, body } => {
                let body = self.block(body, env, in_fn)?;
                out.push(mk(
                    StmtKind::For {
                        var: var.clone(),
                        iter: iter.clone(),
                        body,
                    },
                    span,
                ));
            }
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                let body = self.block(body, env, in_fn)?;
                let except = match except {
                    Some((n, b)) => Some((n.clone(), self.block(b, env, in_fn)?)),
                    None => None,
                };
                let orelse = self.opt_block(orelse, env, in_fn)?;
                let finalbody = self.opt_block(finalbody, env, in_fn)?;
                out.push(mk(
                    StmtKind::Try {
                        body,
                        except,
                        orelse,
                        finalbody,
                    },
                    span,
                ));
            }
            StmtKind::With { items, body } => {
                let body = self.block(body, env, in_fn)?;
                out.push(mk(
                    StmtKind::With {
                        items: items.clone(),
                        body,
                    },
                    span,
                ));
            }
            StmtKind::Def {
                name,
                decorators,
                type_params,
                params,
                positional_only,
                keyword_only,
                raises,
                ret,
                body,
            } => {
                // A comptime-dependent generic template can't be elaborated now (its
                // parameter value is unknown); keep it verbatim for monomorphization.
                if self.specializable.contains_key(name) {
                    out.push(stmt.clone());
                    return Ok(());
                }
                let body = self.block(body, env, true)?;
                out.push(mk(
                    StmtKind::Def {
                        name: name.clone(),
                        decorators: decorators.clone(),
                        type_params: type_params.clone(),
                        params: params.clone(),
                        positional_only: *positional_only,
                        keyword_only: *keyword_only,
                        raises: *raises,
                        ret: ret.clone(),
                        body,
                    },
                    span,
                ));
            }
            StmtKind::Struct {
                name,
                decorators,
                type_params,
                conforms,
                fields,
                associated,
                methods,
                fieldwise_init,
            } => {
                let methods = methods
                    .iter()
                    .map(|m| {
                        let mut m = m.clone();
                        m.body = self.block(&m.body, env, true)?;
                        Ok(m)
                    })
                    .collect::<Result<Vec<_>, ComptimeError>>()?;
                out.push(mk(
                    StmtKind::Struct {
                        name: name.clone(),
                        decorators: decorators.clone(),
                        type_params: type_params.clone(),
                        conforms: conforms.clone(),
                        fields: fields.clone(),
                        associated: associated.clone(),
                        methods,
                        fieldwise_init: *fieldwise_init,
                    },
                    span,
                ));
            }
            _ => out.push(stmt.clone()),
        }
        Ok(())
    }

    fn opt_block(
        &self,
        block: &Option<Vec<Stmt>>,
        env: &mut HashMap<String, CtValue>,
        in_fn: bool,
    ) -> Result<Option<Vec<Stmt>>, ComptimeError> {
        match block {
            Some(b) => Ok(Some(self.block(b, env, in_fn)?)),
            None => Ok(None),
        }
    }

    // --- Compile-time evaluation --------------------------------------------

    /// Evaluate a compile-time expression to a `CtValue`. `scope` is the current
    /// variable environment (module constants, or a CTFE call frame's locals).
    fn eval(&self, e: &Expr, scope: &HashMap<String, CtValue>) -> Result<CtValue, ComptimeError> {
        match &e.kind {
            ExprKind::Int(n) => Ok(CtValue::Int(*n)),
            ExprKind::Bool(b) => Ok(CtValue::Bool(*b)),
            ExprKind::Str(s) => Ok(CtValue::Str(s.clone())),
            ExprKind::Identifier(name) => {
                if let Some(value) = scope.get(name) {
                    return Ok(value.clone());
                }
                self.type_value(name, &[], scope)
            }
            ExprKind::TypeApply { name, args } => self.type_value(name, args, scope),
            ExprKind::TupleLit(elems) => Ok(CtValue::Tuple(self.eval_all(elems, scope)?)),
            ExprKind::ListLit(elems) => Ok(CtValue::List(self.eval_all(elems, scope)?)),
            ExprKind::Member { object, field } => {
                if let ExprKind::Identifier(name) = &object.kind
                    && name == "Self"
                    && let Some(value) = scope.get(field)
                {
                    return Ok(value.clone());
                }
                match self.eval(object, scope)? {
                    CtValue::Type(ty) => self.associated_value(&ty, field),
                    _ => Err(ComptimeError::NotComptime(format!(
                        "compile-time member access '.{field}' needs a type value"
                    ))),
                }
            }
            ExprKind::Index { object, index } => {
                let seq = self
                    .eval(object, scope)?
                    .as_sequence("indexing a comptime collection")?;
                let i = self.eval(index, scope)?.as_int("comptime index")?;
                seq.get(i as usize).cloned().ok_or_else(|| {
                    ComptimeError::BadArithmetic(format!("comptime index {i} out of range"))
                })
            }
            ExprKind::Prefix(PrefixOp::Neg, inner) => {
                Ok(CtValue::Int(-self.eval(inner, scope)?.as_int("unary '-'")?))
            }
            ExprKind::Prefix(PrefixOp::Not, inner) => {
                Ok(CtValue::Bool(!self.eval(inner, scope)?.as_bool("'not'")?))
            }
            ExprKind::Infix(op, l, r) => self.eval_infix(*op, l, r, scope),
            ExprKind::Compare { first, rest } => {
                let mut left = self.eval(first, scope)?.as_int("chained comparison")?;
                for (op, right) in rest {
                    let r = self.eval(right, scope)?.as_int("chained comparison")?;
                    if !compare_ints(*op, left, r)? {
                        return Ok(CtValue::Bool(false));
                    }
                    left = r;
                }
                Ok(CtValue::Bool(true))
            }
            // A built-in compile-time **type predicate** (Phase 7): `is_same_type[T,
            // U]()` is `Bool` type equality, usable in a `comptime if`.
            ExprKind::Call {
                name,
                param_args,
                args,
                ..
            } if name == "is_same_type" => self.eval_is_same_type(param_args, args, scope),
            // A call into a pure top-level function → CTFE.
            ExprKind::Call {
                name,
                param_args,
                args,
                ..
            } => {
                let argv = self.eval_all(args, scope)?;
                self.ctfe_call(name, param_args, argv, scope)
            }
            _ => Err(ComptimeError::NotComptime(
                "unsupported compile-time expression".to_string(),
            )),
        }
    }

    fn eval_all(
        &self,
        exprs: &[Expr],
        scope: &HashMap<String, CtValue>,
    ) -> Result<Vec<CtValue>, ComptimeError> {
        exprs.iter().map(|e| self.eval(e, scope)).collect()
    }

    fn eval_infix(
        &self,
        op: InfixOp,
        l: &Expr,
        r: &Expr,
        scope: &HashMap<String, CtValue>,
    ) -> Result<CtValue, ComptimeError> {
        match op {
            InfixOp::And => {
                return Ok(CtValue::Bool(
                    self.eval(l, scope)?.as_bool("'and'")?
                        && self.eval(r, scope)?.as_bool("'and'")?,
                ));
            }
            InfixOp::Or => {
                return Ok(CtValue::Bool(
                    self.eval(l, scope)?.as_bool("'or'")?
                        || self.eval(r, scope)?.as_bool("'or'")?,
                ));
            }
            _ => {}
        }
        // String concatenation (`+`) and equality (`==`/`!=`) at compile time.
        if let (CtValue::Str(a), CtValue::Str(b)) = (self.eval(l, scope)?, self.eval(r, scope)?) {
            return match op {
                InfixOp::Add => Ok(CtValue::Str(a + &b)),
                InfixOp::Eq => Ok(CtValue::Bool(a == b)),
                InfixOp::Ne => Ok(CtValue::Bool(a != b)),
                _ => Err(ComptimeError::NotComptime(
                    "unsupported compile-time String operator".to_string(),
                )),
            };
        }
        let a = self.eval(l, scope)?.as_int("integer operator")?;
        let b = self.eval(r, scope)?.as_int("integer operator")?;
        use InfixOp::*;
        let bad = |m: &str| ComptimeError::BadArithmetic(m.to_string());
        match op {
            Add => Ok(CtValue::Int(a + b)),
            Sub => Ok(CtValue::Int(a - b)),
            Mul => Ok(CtValue::Int(a * b)),
            FloorDiv if b != 0 => Ok(CtValue::Int(a.div_euclid(b))),
            Mod if b != 0 => Ok(CtValue::Int(a.rem_euclid(b))),
            FloorDiv | Mod => Err(bad("division by zero")),
            Pow if b >= 0 => Ok(CtValue::Int(a.pow(b as u32))),
            Pow => Err(bad("negative exponent")),
            Eq | Ne | Lt | Gt | Le | Ge => Ok(CtValue::Bool(compare_ints(op, a, b)?)),
            _ => Err(ComptimeError::NotComptime(
                "unsupported compile-time operator".to_string(),
            )),
        }
    }

    /// Evaluate a `comptime for` / CTFE `for` iterable to the sequence of loop
    /// values: a `range(...)` of `Int`s, or any compile-time tuple/list.
    fn eval_iter(
        &self,
        iter: &Expr,
        scope: &HashMap<String, CtValue>,
    ) -> Result<Vec<CtValue>, ComptimeError> {
        if let ExprKind::Call { name, args, .. } = &iter.kind
            && name == "range"
        {
            let vals: Vec<i64> = args
                .iter()
                .map(|a| self.eval(a, scope)?.as_int("range argument"))
                .collect::<Result<_, _>>()?;
            let (start, stop, step) = match vals.as_slice() {
                [stop] => (0, *stop, 1),
                [start, stop] => (*start, *stop, 1),
                [start, stop, step] => (*start, *stop, *step),
                _ => {
                    return Err(ComptimeError::BadRange(
                        "range takes 1-3 arguments".to_string(),
                    ));
                }
            };
            if step == 0 {
                return Err(ComptimeError::BadRange(
                    "range step cannot be zero".to_string(),
                ));
            }
            let mut out = Vec::new();
            let mut i = start;
            while (step > 0 && i < stop) || (step < 0 && i > stop) {
                out.push(CtValue::Int(i));
                i += step;
            }
            return Ok(out);
        }
        self.eval(iter, scope)?
            .as_sequence("a range(...), tuple, or list")
    }

    // --- CTFE: run a pure function at compile time --------------------------

    fn ctfe_call(
        &self,
        name: &str,
        param_args: &[ParamArg],
        args: Vec<CtValue>,
        scope: &HashMap<String, CtValue>,
    ) -> Result<CtValue, ComptimeError> {
        let f = self.fns.get(name).ok_or_else(|| {
            ComptimeError::NotComptime(format!("'{name}' is not a compile-time-callable function"))
        })?;
        if f.ct_params.len() != param_args.len() {
            return Err(ComptimeError::Arity(format!(
                "'{name}' expects {} compile-time argument(s), got {}",
                f.ct_params.len(),
                param_args.len()
            )));
        }
        if f.params.len() != args.len() {
            return Err(ComptimeError::Arity(format!(
                "'{name}' expects {} argument(s), got {}",
                f.params.len(),
                args.len()
            )));
        }
        self.burn()?;
        let mut locals: HashMap<String, CtValue> = HashMap::new();
        let mut value_params = Vec::new();
        for (decl, arg) in f.ct_params.iter().zip(param_args) {
            let value = self.resolve_ct_arg(decl, arg, scope)?;
            if let ParamDecl::Value { name } = decl
                && !matches!(value, CtValue::Type(_))
            {
                value_params.push((name.clone(), ct_to_vm(&value)?));
            }
            locals.insert(decl.name().to_string(), value);
        }
        locals.extend(f.params.iter().cloned().zip(args));
        let mut visiting = HashSet::new();
        let mut needed = HashSet::new();
        if self.vm_ctfe_safe_fn(name, &mut visiting, &mut needed)
            && let Some(value) = self.vm_ctfe_call(name, &locals, &value_params, &needed)?
        {
            return Ok(value);
        }
        Err(ComptimeError::NotComptime(format!(
            "'{name}' is not safe for VM-backed compile-time execution"
        )))
    }

    fn vm_ctfe_call(
        &self,
        name: &str,
        locals: &HashMap<String, CtValue>,
        value_params: &[(String, Value)],
        needed: &HashSet<String>,
    ) -> Result<Option<CtValue>, ComptimeError> {
        let Some(f) = self.fns.get(name) else {
            return Ok(None);
        };
        let mut args = Vec::with_capacity(f.params.len());
        for pname in &f.params {
            let value = locals.get(pname).ok_or_else(|| {
                ComptimeError::NotComptime(format!(
                    "missing compile-time argument '{pname}' for VM CTFE call"
                ))
            })?;
            args.push(ct_to_vm(value)?);
        }
        let mut vm = VmBackend::new();
        let mut program = self
            .program
            .iter()
            .filter(
                |stmt| matches!(&stmt.kind, StmtKind::Def { name, .. } if needed.contains(name)),
            )
            .cloned()
            .collect::<Vec<_>>();
        self.rewrite_vm_ctfe_program(&mut program, name, locals)?;
        if program.is_empty() {
            return Err(ComptimeError::NotComptime(format!(
                "missing compile-time function '{name}' for VM CTFE"
            )));
        }
        program.sort_by_key(|stmt| match &stmt.kind {
            StmtKind::Def { name: def_name, .. } if def_name == name => 0,
            _ => 1,
        });
        let (value, remaining_fuel) = vm
            .run_function_value(&program, name, args, value_params, self.fuel.get())
            .map_err(|e| ComptimeError::NotComptime(format!("VM CTFE failed for '{name}': {e}")))?;
        self.fuel.set(remaining_fuel);
        Ok(Some(vm_to_ct(value)?))
    }

    fn rewrite_vm_ctfe_program(
        &self,
        program: &mut [Stmt],
        root: &str,
        root_scope: &HashMap<String, CtValue>,
    ) -> Result<(), ComptimeError> {
        for stmt in program {
            let scope = match &stmt.kind {
                StmtKind::Def { name, .. } if name == root => root_scope,
                _ => {
                    // Non-root helpers with only runtime-value parameters need no
                    // type-fact substitution; recursive value-parameter calls are
                    // handled by the VM's normal value-param reification.
                    continue;
                }
            };
            self.rewrite_vm_ctfe_stmt(stmt, scope)?;
        }
        Ok(())
    }

    fn rewrite_vm_ctfe_block(
        &self,
        stmts: &mut [Stmt],
        scope: &HashMap<String, CtValue>,
    ) -> Result<(), ComptimeError> {
        for stmt in stmts {
            self.rewrite_vm_ctfe_stmt(stmt, scope)?;
        }
        Ok(())
    }

    fn rewrite_vm_ctfe_stmt(
        &self,
        stmt: &mut Stmt,
        scope: &HashMap<String, CtValue>,
    ) -> Result<(), ComptimeError> {
        match &mut stmt.kind {
            StmtKind::Def { body, .. } => self.rewrite_vm_ctfe_block(body, scope),
            StmtKind::VarDecl { value, .. }
            | StmtKind::RefDecl { value, .. }
            | StmtKind::Assign { value, .. } => self.rewrite_vm_ctfe_expr(value, scope),
            StmtKind::AugAssign { place, value, .. } | StmtKind::SetPlace { place, value } => {
                self.rewrite_vm_ctfe_expr(place, scope)?;
                self.rewrite_vm_ctfe_expr(value, scope)
            }
            StmtKind::Return(Some(value)) | StmtKind::Expr(value) => {
                self.rewrite_vm_ctfe_expr(value, scope)
            }
            StmtKind::If { branches, orelse } => {
                for (cond, body) in branches {
                    self.rewrite_vm_ctfe_expr(cond, scope)?;
                    self.rewrite_vm_ctfe_block(body, scope)?;
                }
                if let Some(body) = orelse {
                    self.rewrite_vm_ctfe_block(body, scope)?;
                }
                Ok(())
            }
            StmtKind::While { cond, body } => {
                self.rewrite_vm_ctfe_expr(cond, scope)?;
                self.rewrite_vm_ctfe_block(body, scope)
            }
            StmtKind::For { iter, body, .. } => {
                self.rewrite_vm_ctfe_expr(iter, scope)?;
                self.rewrite_vm_ctfe_block(body, scope)
            }
            StmtKind::Return(None) | StmtKind::Pass => Ok(()),
            _ => Ok(()),
        }
    }

    fn rewrite_vm_ctfe_expr(
        &self,
        expr: &mut Expr,
        scope: &HashMap<String, CtValue>,
    ) -> Result<(), ComptimeError> {
        match &mut expr.kind {
            ExprKind::Call {
                name,
                param_args,
                args,
                kwargs,
            } if name == "is_same_type" => {
                for arg in param_args.iter_mut() {
                    if let ParamArg::Value(e) = arg {
                        self.rewrite_vm_ctfe_expr(e, scope)?;
                    }
                }
                for arg in args.iter_mut() {
                    self.rewrite_vm_ctfe_expr(arg, scope)?;
                }
                for kw in kwargs.iter_mut() {
                    self.rewrite_vm_ctfe_expr(&mut kw.value, scope)?;
                }
                let value = self.eval_is_same_type(param_args, args, scope)?;
                *expr = lit_result(&value, expr.span)?;
                Ok(())
            }
            ExprKind::Member { object, .. } => {
                self.rewrite_vm_ctfe_expr(object, scope)?;
                if let Ok(value) = self.eval(expr, scope)
                    && let Some(materialized) = value.materialize(expr.span)
                {
                    *expr = materialized;
                }
                Ok(())
            }
            ExprKind::Prefix(_, inner) | ExprKind::Transfer(inner) => {
                self.rewrite_vm_ctfe_expr(inner, scope)
            }
            ExprKind::Infix(_, left, right) => {
                self.rewrite_vm_ctfe_expr(left, scope)?;
                self.rewrite_vm_ctfe_expr(right, scope)
            }
            ExprKind::Call {
                param_args,
                args,
                kwargs,
                ..
            } => {
                for arg in param_args.iter_mut() {
                    if let ParamArg::Value(e) = arg {
                        self.rewrite_vm_ctfe_expr(e, scope)?;
                    }
                }
                for arg in args.iter_mut() {
                    self.rewrite_vm_ctfe_expr(arg, scope)?;
                }
                for kw in kwargs.iter_mut() {
                    self.rewrite_vm_ctfe_expr(&mut kw.value, scope)?;
                }
                Ok(())
            }
            ExprKind::MethodCall {
                object,
                args,
                kwargs,
                ..
            } => {
                self.rewrite_vm_ctfe_expr(object, scope)?;
                for arg in args.iter_mut() {
                    self.rewrite_vm_ctfe_expr(arg, scope)?;
                }
                for kw in kwargs.iter_mut() {
                    self.rewrite_vm_ctfe_expr(&mut kw.value, scope)?;
                }
                Ok(())
            }
            ExprKind::Index { object, index } => {
                self.rewrite_vm_ctfe_expr(object, scope)?;
                self.rewrite_vm_ctfe_expr(index, scope)
            }
            ExprKind::Slice {
                object,
                lower,
                upper,
                step,
            } => {
                self.rewrite_vm_ctfe_expr(object, scope)?;
                for bound in [lower, upper, step].into_iter().flatten() {
                    self.rewrite_vm_ctfe_expr(bound, scope)?;
                }
                Ok(())
            }
            ExprKind::ListLit(items) | ExprKind::TupleLit(items) => {
                for item in items {
                    self.rewrite_vm_ctfe_expr(item, scope)?;
                }
                Ok(())
            }
            ExprKind::Named { value, .. } => self.rewrite_vm_ctfe_expr(value, scope),
            ExprKind::IfExpr {
                cond,
                then_branch,
                else_branch,
            } => {
                self.rewrite_vm_ctfe_expr(cond, scope)?;
                self.rewrite_vm_ctfe_expr(then_branch, scope)?;
                self.rewrite_vm_ctfe_expr(else_branch, scope)
            }
            ExprKind::Compare { first, rest } => {
                self.rewrite_vm_ctfe_expr(first, scope)?;
                for (_, e) in rest {
                    self.rewrite_vm_ctfe_expr(e, scope)?;
                }
                Ok(())
            }
            ExprKind::TString { parts, .. } => {
                for part in parts {
                    if let crate::ast::TStringPart::Expr(e) = part {
                        self.rewrite_vm_ctfe_expr(e, scope)?;
                    }
                }
                Ok(())
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::None
            | ExprKind::Identifier(_)
            | ExprKind::TypeValue(_)
            | ExprKind::Invoke { .. }
            | ExprKind::BraceLit(_)
            | ExprKind::TypeApply { .. } => Ok(()),
        }
    }

    fn vm_ctfe_safe_fn(
        &self,
        name: &str,
        visiting: &mut HashSet<String>,
        needed: &mut HashSet<String>,
    ) -> bool {
        if needed.contains(name) {
            return true;
        }
        if !visiting.insert(name.to_string()) {
            needed.insert(name.to_string());
            return true;
        }
        let Some(f) = self.fns.get(name) else {
            visiting.remove(name);
            return false;
        };
        let safe = self.vm_ctfe_safe_block(f.body, visiting, needed);
        visiting.remove(name);
        if safe {
            needed.insert(name.to_string());
        }
        safe
    }

    fn vm_ctfe_safe_block(
        &self,
        stmts: &[Stmt],
        visiting: &mut HashSet<String>,
        needed: &mut HashSet<String>,
    ) -> bool {
        stmts
            .iter()
            .all(|s| self.vm_ctfe_safe_stmt(s, visiting, needed))
    }

    fn vm_ctfe_safe_stmt(
        &self,
        stmt: &Stmt,
        visiting: &mut HashSet<String>,
        needed: &mut HashSet<String>,
    ) -> bool {
        match &stmt.kind {
            StmtKind::VarDecl { value, .. }
            | StmtKind::RefDecl { value, .. }
            | StmtKind::Assign { value, .. } => self.vm_ctfe_safe_expr(value, visiting, needed),
            StmtKind::AugAssign { place, value, .. } | StmtKind::SetPlace { place, value } => {
                self.vm_ctfe_safe_expr(place, visiting, needed)
                    && self.vm_ctfe_safe_expr(value, visiting, needed)
            }
            StmtKind::Return(Some(value)) | StmtKind::Expr(value) => {
                self.vm_ctfe_safe_expr(value, visiting, needed)
            }
            StmtKind::Return(None) | StmtKind::Pass => true,
            StmtKind::If { branches, orelse } => {
                branches.iter().all(|(cond, body)| {
                    self.vm_ctfe_safe_expr(cond, visiting, needed)
                        && self.vm_ctfe_safe_block(body, visiting, needed)
                }) && orelse
                    .as_ref()
                    .is_none_or(|body| self.vm_ctfe_safe_block(body, visiting, needed))
            }
            StmtKind::While { cond, body } => {
                self.vm_ctfe_safe_expr(cond, visiting, needed)
                    && self.vm_ctfe_safe_block(body, visiting, needed)
            }
            StmtKind::For { iter, body, .. } => {
                self.vm_ctfe_safe_expr(iter, visiting, needed)
                    && self.vm_ctfe_safe_block(body, visiting, needed)
            }
            StmtKind::ComptimeIf { .. }
            | StmtKind::ComptimeFor { .. }
            | StmtKind::Raise(_)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Def { .. }
            | StmtKind::Struct { .. }
            | StmtKind::Trait { .. }
            | StmtKind::Import { .. }
            | StmtKind::FromImport { .. }
            | StmtKind::With { .. }
            | StmtKind::Try { .. }
            | StmtKind::Unpack { .. }
            | StmtKind::Comptime { .. } => false,
        }
    }

    fn vm_ctfe_safe_expr(
        &self,
        expr: &Expr,
        visiting: &mut HashSet<String>,
        needed: &mut HashSet<String>,
    ) -> bool {
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::None
            | ExprKind::Identifier(_) => true,
            ExprKind::Prefix(_, inner) | ExprKind::Transfer(inner) => {
                self.vm_ctfe_safe_expr(inner, visiting, needed)
            }
            ExprKind::Infix(_, left, right) => {
                self.vm_ctfe_safe_expr(left, visiting, needed)
                    && self.vm_ctfe_safe_expr(right, visiting, needed)
            }
            ExprKind::TupleLit(items) | ExprKind::ListLit(items) => items
                .iter()
                .all(|e| self.vm_ctfe_safe_expr(e, visiting, needed)),
            ExprKind::Index { object, index } => {
                self.vm_ctfe_safe_expr(object, visiting, needed)
                    && self.vm_ctfe_safe_expr(index, visiting, needed)
            }
            ExprKind::Member { object, .. } => self.vm_ctfe_safe_expr(object, visiting, needed),
            ExprKind::IfExpr {
                cond,
                then_branch,
                else_branch,
            } => {
                self.vm_ctfe_safe_expr(cond, visiting, needed)
                    && self.vm_ctfe_safe_expr(then_branch, visiting, needed)
                    && self.vm_ctfe_safe_expr(else_branch, visiting, needed)
            }
            ExprKind::Compare { first, rest } => {
                self.vm_ctfe_safe_expr(first, visiting, needed)
                    && rest
                        .iter()
                        .all(|(_, e)| self.vm_ctfe_safe_expr(e, visiting, needed))
            }
            ExprKind::Slice {
                object,
                lower,
                upper,
                step,
            } => {
                self.vm_ctfe_safe_expr(object, visiting, needed)
                    && lower
                        .as_ref()
                        .is_none_or(|e| self.vm_ctfe_safe_expr(e, visiting, needed))
                    && upper
                        .as_ref()
                        .is_none_or(|e| self.vm_ctfe_safe_expr(e, visiting, needed))
                    && step
                        .as_ref()
                        .is_none_or(|e| self.vm_ctfe_safe_expr(e, visiting, needed))
            }
            ExprKind::Call {
                name,
                param_args,
                args,
                kwargs,
            } => {
                kwargs.is_empty()
                    && param_args.iter().all(|arg| match arg {
                        ParamArg::Value(e) => self.vm_ctfe_safe_expr(e, visiting, needed),
                        ParamArg::Type(_) => true,
                    })
                    && args
                        .iter()
                        .all(|e| self.vm_ctfe_safe_expr(e, visiting, needed))
                    && (name == "is_same_type"
                        || vm_ctfe_safe_builtin(name)
                        || self.vm_ctfe_safe_fn(name, visiting, needed))
            }
            ExprKind::MethodCall { .. }
            | ExprKind::BraceLit(_)
            | ExprKind::Invoke { .. }
            | ExprKind::TypeValue(_)
            | ExprKind::TypeApply { .. }
            | ExprKind::Named { .. }
            | ExprKind::TString { .. } => false,
        }
    }

    fn resolve_ct_arg(
        &self,
        decl: &ParamDecl,
        arg: &ParamArg,
        scope: &HashMap<String, CtValue>,
    ) -> Result<CtValue, ComptimeError> {
        match decl {
            ParamDecl::Type { name, .. } => match arg {
                ParamArg::Type(ty) => self
                    .type_from_anno(ty, scope)
                    .map(|ty| CtValue::Type(Box::new(ty))),
                ParamArg::Value(Expr {
                    kind: ExprKind::Identifier(id),
                    ..
                }) => self.type_value(id, &[], scope),
                ParamArg::Value(Expr {
                    kind: ExprKind::TypeApply { name, args },
                    ..
                }) => self.type_value(name, args, scope),
                ParamArg::Value(expr) => Err(ComptimeError::NotComptime(format!(
                    "type parameter '{name}' needs a type argument, got {expr:?}"
                ))),
            },
            ParamDecl::Value { name } => match arg {
                ParamArg::Value(expr) => {
                    let value = self.eval(expr, scope)?;
                    match value {
                        CtValue::Int(_) => Ok(value),
                        _ => Err(ComptimeError::NotInt(format!("value parameter '{name}'"))),
                    }
                }
                ParamArg::Type(_) => {
                    Err(ComptimeError::NotInt(format!("value parameter '{name}'")))
                }
            },
        }
    }

    fn type_value(
        &self,
        name: &str,
        args: &[ParamArg],
        scope: &HashMap<String, CtValue>,
    ) -> Result<CtValue, ComptimeError> {
        self.type_from_name(name, args, scope)
            .map(|ty| CtValue::Type(Box::new(ty)))
    }

    /// The built-in type predicate `is_same_type[T, U]()` (Phase 7): resolve both
    /// type parameters and compare them for equality, yielding a compile-time
    /// `Bool`. Takes exactly two type parameters and no value arguments.
    fn eval_is_same_type(
        &self,
        param_args: &[ParamArg],
        args: &[Expr],
        scope: &HashMap<String, CtValue>,
    ) -> Result<CtValue, ComptimeError> {
        if param_args.len() != 2 || !args.is_empty() {
            return Err(ComptimeError::Arity(
                "is_same_type[T, U]() takes two type parameters and no arguments".to_string(),
            ));
        }
        let a = self.param_arg_type(&param_args[0], scope)?;
        let b = self.param_arg_type(&param_args[1], scope)?;
        Ok(CtValue::Bool(a == b))
    }

    /// Resolve a `[...]` argument that is expected to be a **type** (a type
    /// annotation, a bare type name, or a parameterized type) to a `Ty`.
    fn param_arg_type(
        &self,
        arg: &ParamArg,
        scope: &HashMap<String, CtValue>,
    ) -> Result<Ty, ComptimeError> {
        match arg {
            ParamArg::Type(t) => self.type_from_anno(t, scope),
            ParamArg::Value(Expr {
                kind: ExprKind::Identifier(id),
                ..
            }) => self.type_from_name(id, &[], scope),
            ParamArg::Value(Expr {
                kind: ExprKind::TypeApply { name, args },
                ..
            }) => self.type_from_name(name, args, scope),
            ParamArg::Value(_) => Err(ComptimeError::NotComptime(
                "expected a type argument".to_string(),
            )),
        }
    }

    fn type_from_anno(
        &self,
        ty: &Type,
        scope: &HashMap<String, CtValue>,
    ) -> Result<Ty, ComptimeError> {
        match ty {
            Type::Int => Ok(Ty::Int),
            Type::UInt => Ok(Ty::UInt),
            Type::Bool => Ok(Ty::Bool),
            Type::String => Ok(Ty::String),
            Type::Float64 => Ok(Ty::Float64),
            Type::None => Ok(Ty::None),
            Type::Named(name, args) => self.type_from_name(name, args, scope),
            Type::SelfParam(name) => match scope.get(name) {
                Some(CtValue::Type(ty)) => Ok((**ty).clone()),
                Some(_) => Err(ComptimeError::NotComptime(format!(
                    "Self.{name} is not type-valued"
                ))),
                None => Err(ComptimeError::NotComptime(format!(
                    "unknown compile-time type Self.{name}"
                ))),
            },
            Type::Assoc { base, name } => {
                let base = self.type_from_anno(base, scope)?;
                match self.associated_value(&base, name)? {
                    CtValue::Type(ty) => Ok(*ty),
                    _ => Err(ComptimeError::NotComptime(format!(
                        "{}.{name} is not type-valued",
                        base
                    ))),
                }
            }
            Type::SelfType | Type::Func { .. } | Type::Ref(_) => Err(ComptimeError::NotComptime(
                "unsupported compile-time type argument".to_string(),
            )),
        }
    }

    fn type_from_name(
        &self,
        name: &str,
        args: &[ParamArg],
        scope: &HashMap<String, CtValue>,
    ) -> Result<Ty, ComptimeError> {
        if args.is_empty() {
            if let Some(CtValue::Type(ty)) = scope.get(name) {
                return Ok((**ty).clone());
            }
            if let Some(ty) = scalar_type_name(name) {
                return Ok(ty);
            }
        }
        let Some(info) = self.structs.get(name) else {
            return Err(ComptimeError::NotComptime(format!(
                "'{name}' is not a compile-time type"
            )));
        };
        if args.len() != info.decls.len() {
            return Err(ComptimeError::Arity(format!(
                "type '{name}' expects {} compile-time argument(s), got {}",
                info.decls.len(),
                args.len()
            )));
        }
        let tyargs = info
            .decls
            .iter()
            .zip(args)
            .map(|(decl, arg)| {
                let value = self.resolve_ct_arg(decl, arg, scope)?;
                match (decl, value) {
                    (ParamDecl::Type { .. }, CtValue::Type(ty)) => Ok(TyArg::Ty(*ty)),
                    (ParamDecl::Type { name, .. }, _) => Err(ComptimeError::NotComptime(format!(
                        "type parameter '{name}' needs a type argument"
                    ))),
                    (ParamDecl::Value { .. }, value) => Ok(TyArg::Val(value)),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Ty::Struct(name.to_string(), tyargs))
    }

    fn associated_value(&self, base: &Ty, member: &str) -> Result<CtValue, ComptimeError> {
        let Ty::Struct(name, args) = base else {
            return Err(ComptimeError::NotComptime(format!(
                "type '{base}' has no compile-time member '{member}'"
            )));
        };
        let info = self.structs.get(name).ok_or_else(|| {
            ComptimeError::NotComptime(format!("unknown compile-time struct '{name}'"))
        })?;
        let assoc = info
            .associated
            .iter()
            .find(|a| a.name == member)
            .ok_or_else(|| {
                ComptimeError::NotComptime(format!(
                    "type '{base}' has no compile-time member '{member}'"
                ))
            })?;
        let mut env = HashMap::new();
        for (decl, arg) in info.decls.iter().zip(args) {
            match (decl, arg) {
                (ParamDecl::Type { name, .. }, TyArg::Ty(ty)) => {
                    env.insert(name.clone(), CtValue::Type(Box::new(ty.clone())));
                }
                (ParamDecl::Value { name }, TyArg::Val(value)) => {
                    env.insert(name.clone(), value.clone());
                }
                _ => {}
            }
        }
        self.eval(&assoc.value, &env)
    }

    // --- Monomorphization of comptime-dependent generics (Phase 6) -----------

    /// Specialize every comptime-dependent generic template against the value
    /// arguments at its call sites, replacing each template with its concrete
    /// specializations (which have their `comptime if`/`for` resolved).
    fn monomorphize(&self, program: Vec<Stmt>) -> Result<Vec<Stmt>, ComptimeError> {
        if self.specializable.is_empty() {
            return Ok(program);
        }
        let consts = self.top_consts.borrow().clone();
        let mut mono = Mono::default();
        let mut program = program;
        // Rewrite call sites in every non-template statement, seeding the worklist.
        for stmt in program.iter_mut() {
            if let StmtKind::Def { name, .. } = &stmt.kind
                && self.specializable.contains_key(name)
            {
                continue; // a template — replaced wholesale below
            }
            self.mono_stmt(stmt, &consts, &mut mono)?;
        }
        // Drain the worklist, generating each requested specialization and scanning
        // its body for further (e.g. recursive) instantiations.
        while let Some(job) = mono.queue.pop_front() {
            let mut def = self.generate_spec(&job.orig, &job.vals)?;
            if let StmtKind::Def { body, .. } = &mut def.kind {
                self.mono_block(body, &consts, &mut mono)?;
            }
            mono.generated.entry(job.orig).or_default().push(def);
        }
        // Rebuild the program, replacing each template with its specializations at
        // the template's original position. Specializations are emitted in reverse
        // generation order so a callee is defined before its caller (the checker
        // binds names sequentially, without forward references).
        let mut out = Vec::with_capacity(program.len());
        for stmt in program {
            match &stmt.kind {
                StmtKind::Def { name, .. } if self.specializable.contains_key(name) => {
                    if let Some(mut specs) = mono.generated.remove(name) {
                        specs.reverse();
                        out.extend(specs);
                    }
                    // No call sites ⇒ dead generic template, dropped.
                }
                _ => out.push(stmt),
            }
        }
        Ok(out)
    }

    /// Generate one specialization of template `orig` for the compile-time arguments
    /// `vals` (in parameter order): bind every parameter in the comptime env so
    /// `comptime if`/`for` resolve against the concrete arguments, then fold **value**
    /// parameters into runtime literals and drop them from the signature, while
    /// **type** parameters stay symbolic (the specialized def is still type-generic,
    /// checked the usual erased way — only its comptime branches were selected).
    fn generate_spec(&self, orig: &str, vals: &[CtValue]) -> Result<Stmt, ComptimeError> {
        let template = self.specializable[orig];
        let StmtKind::Def {
            decorators,
            type_params,
            params,
            positional_only,
            keyword_only,
            raises,
            ret,
            body,
            ..
        } = &template.kind
        else {
            return Err(ComptimeError::NotComptime(format!(
                "specialization registry entry '{orig}' is not a function"
            )));
        };
        let decls = classify_ct_params(type_params);
        if decls.len() != vals.len() {
            return Err(ComptimeError::Arity(format!(
                "'{orig}' expects {} compile-time argument(s), got {}",
                decls.len(),
                vals.len()
            )));
        }
        // Bind every parameter for comptime resolution; fold value parameters into
        // runtime literals (except where a regular parameter shadows the name); keep
        // type parameters on the specialized signature.
        let mut env = self.top_consts.borrow().clone();
        let mut subs = self.top_consts.borrow().clone();
        for p in params {
            subs.remove(&p.name);
        }
        let mut kept_type_params = Vec::new();
        for ((decl, tp), v) in decls.iter().zip(type_params).zip(vals) {
            env.insert(decl.name().to_string(), v.clone());
            match decl {
                ParamDecl::Value { name } => {
                    subs.insert(name.clone(), v.clone());
                }
                ParamDecl::Type { .. } => kept_type_params.push(tp.clone()),
            }
        }
        // Elaborate the body with the parameters bound, so its comptime constructs
        // select/unroll against the concrete arguments.
        let elaborated = self.block(body, &mut env, true)?;
        let final_body = materialize_block(elaborated, &subs);
        Ok(mk(
            StmtKind::Def {
                name: mangle(orig, vals),
                decorators: decorators.clone(),
                type_params: kept_type_params,
                params: params.clone(),
                positional_only: *positional_only,
                keyword_only: *keyword_only,
                raises: *raises,
                ret: ret.clone(),
                body: final_body,
            },
            template.span,
        ))
    }

    fn mono_block(
        &self,
        stmts: &mut [Stmt],
        consts: &HashMap<String, CtValue>,
        mono: &mut Mono,
    ) -> Result<(), ComptimeError> {
        for s in stmts {
            self.mono_stmt(s, consts, mono)?;
        }
        Ok(())
    }

    fn mono_stmt(
        &self,
        s: &mut Stmt,
        consts: &HashMap<String, CtValue>,
        mono: &mut Mono,
    ) -> Result<(), ComptimeError> {
        match &mut s.kind {
            StmtKind::VarDecl { value, .. }
            | StmtKind::RefDecl { value, .. }
            | StmtKind::Assign { value, .. }
            | StmtKind::Comptime { value, .. }
            | StmtKind::Raise(value)
            | StmtKind::Return(Some(value)) => self.mono_expr(value, consts, mono),
            StmtKind::Return(None)
            | StmtKind::Pass
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Import { .. }
            | StmtKind::FromImport { .. }
            | StmtKind::Trait { .. } => Ok(()),
            StmtKind::SetPlace { place, value } | StmtKind::AugAssign { place, value, .. } => {
                self.mono_expr(place, consts, mono)?;
                self.mono_expr(value, consts, mono)
            }
            StmtKind::Unpack { targets, value } => {
                for t in targets.iter_mut() {
                    self.mono_expr(t, consts, mono)?;
                }
                self.mono_expr(value, consts, mono)
            }
            StmtKind::Expr(e) => self.mono_expr(e, consts, mono),
            StmtKind::If { branches, orelse } | StmtKind::ComptimeIf { branches, orelse } => {
                for (c, b) in branches.iter_mut() {
                    self.mono_expr(c, consts, mono)?;
                    self.mono_block(b, consts, mono)?;
                }
                if let Some(b) = orelse {
                    self.mono_block(b, consts, mono)?;
                }
                Ok(())
            }
            StmtKind::While { cond, body } => {
                self.mono_expr(cond, consts, mono)?;
                self.mono_block(body, consts, mono)
            }
            StmtKind::For { iter, body, .. } | StmtKind::ComptimeFor { iter, body, .. } => {
                self.mono_expr(iter, consts, mono)?;
                self.mono_block(body, consts, mono)
            }
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                self.mono_block(body, consts, mono)?;
                if let Some((_, b)) = except {
                    self.mono_block(b, consts, mono)?;
                }
                if let Some(b) = orelse {
                    self.mono_block(b, consts, mono)?;
                }
                if let Some(b) = finalbody {
                    self.mono_block(b, consts, mono)?;
                }
                Ok(())
            }
            StmtKind::With { items, body } => {
                for WithItem { context, .. } in items.iter_mut() {
                    self.mono_expr(context, consts, mono)?;
                }
                self.mono_block(body, consts, mono)
            }
            StmtKind::Def { body, .. } => self.mono_block(body, consts, mono),
            StmtKind::Struct { methods, .. } => {
                for m in methods.iter_mut() {
                    self.mono_block(&mut m.body, consts, mono)?;
                }
                Ok(())
            }
        }
    }

    fn mono_expr(
        &self,
        e: &mut Expr,
        consts: &HashMap<String, CtValue>,
        mono: &mut Mono,
    ) -> Result<(), ComptimeError> {
        match &mut e.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::None
            | ExprKind::Identifier(_)
            | ExprKind::TString { .. }
            | ExprKind::TypeApply { .. } => Ok(()),
            ExprKind::Prefix(_, inner) | ExprKind::Transfer(inner) => {
                self.mono_expr(inner, consts, mono)
            }
            ExprKind::Infix(_, l, r) => {
                self.mono_expr(l, consts, mono)?;
                self.mono_expr(r, consts, mono)
            }
            ExprKind::Compare { first, rest } => {
                self.mono_expr(first, consts, mono)?;
                for (_, r) in rest.iter_mut() {
                    self.mono_expr(r, consts, mono)?;
                }
                Ok(())
            }
            ExprKind::Call {
                name,
                param_args,
                args,
                kwargs,
            } => {
                for a in args.iter_mut() {
                    self.mono_expr(a, consts, mono)?;
                }
                for k in kwargs.iter_mut() {
                    self.mono_expr(&mut k.value, consts, mono)?;
                }
                if self.specializable.contains_key(name) {
                    let (vals, kept_type_args) =
                        self.resolve_spec_args(name, param_args, consts)?;
                    let mangled = mangle(name, &vals);
                    if mono.done.insert(mangled.clone()) {
                        mono.queue.push_back(Job {
                            orig: name.clone(),
                            vals,
                        });
                    }
                    *name = mangled;
                    // Value arguments are baked into the specialization; type
                    // arguments stay on the (still type-generic) specialized def.
                    *param_args = kept_type_args;
                }
                Ok(())
            }
            ExprKind::Member { object, .. } => self.mono_expr(object, consts, mono),
            ExprKind::MethodCall {
                object,
                args,
                kwargs,
                ..
            } => {
                self.mono_expr(object, consts, mono)?;
                for a in args.iter_mut() {
                    self.mono_expr(a, consts, mono)?;
                }
                for k in kwargs.iter_mut() {
                    self.mono_expr(&mut k.value, consts, mono)?;
                }
                Ok(())
            }
            ExprKind::Index { object, index } => {
                self.mono_expr(object, consts, mono)?;
                self.mono_expr(index, consts, mono)
            }
            ExprKind::Slice {
                object,
                lower,
                upper,
                step,
            } => {
                self.mono_expr(object, consts, mono)?;
                for b in [lower, upper, step].into_iter().flatten() {
                    self.mono_expr(b, consts, mono)?;
                }
                Ok(())
            }
            ExprKind::ListLit(elems) | ExprKind::TupleLit(elems) => {
                for el in elems.iter_mut() {
                    self.mono_expr(el, consts, mono)?;
                }
                Ok(())
            }
            ExprKind::Named { value, .. } => self.mono_expr(value, consts, mono),
            ExprKind::TypeValue(_) => Ok(()),
            ExprKind::Invoke { .. } => Ok(()),
            ExprKind::BraceLit(_) => Ok(()),
            ExprKind::IfExpr {
                cond,
                then_branch,
                else_branch,
            } => {
                self.mono_expr(cond, consts, mono)?;
                self.mono_expr(then_branch, consts, mono)?;
                self.mono_expr(else_branch, consts, mono)
            }
        }
    }

    /// The classified compile-time parameters of a specializable template.
    fn template_decls(&self, name: &str) -> Result<Vec<ParamDecl>, ComptimeError> {
        match &self.specializable[name].kind {
            StmtKind::Def { type_params, .. } => Ok(classify_ct_params(type_params)),
            _ => Err(ComptimeError::NotComptime(format!(
                "specialization registry entry '{name}' is not a function"
            ))),
        }
    }

    /// Resolve a specializable call's `[...]` arguments into `(ct_values,
    /// kept_type_args)`: each argument is evaluated per its declared parameter kind
    /// — a value parameter to a compile-time value, a type parameter to a
    /// `CtValue::Type`. The type arguments are also returned verbatim to stay on the
    /// specialized (still type-generic) def; value arguments are baked in.
    fn resolve_spec_args(
        &self,
        name: &str,
        param_args: &[ParamArg],
        consts: &HashMap<String, CtValue>,
    ) -> Result<(Vec<CtValue>, Vec<ParamArg>), ComptimeError> {
        let decls = self.template_decls(name)?;
        if param_args.len() != decls.len() {
            return Err(ComptimeError::Arity(format!(
                "generic '{name}' must be called with {} explicit argument(s), got {}",
                decls.len(),
                param_args.len()
            )));
        }
        let mut vals = Vec::with_capacity(decls.len());
        let mut kept_type_args = Vec::new();
        for (decl, arg) in decls.iter().zip(param_args) {
            match decl {
                ParamDecl::Value { name: pn } => match arg {
                    ParamArg::Value(expr) => vals.push(self.eval(expr, consts)?),
                    ParamArg::Type(_) => {
                        return Err(ComptimeError::NotInt(format!("value parameter '{pn}'")));
                    }
                },
                ParamDecl::Type { .. } => {
                    let ty = self.param_arg_type(arg, consts)?;
                    vals.push(CtValue::Type(Box::new(ty)));
                    kept_type_args.push(arg.clone());
                }
            }
        }
        Ok((vals, kept_type_args))
    }
}

/// A pending specialization request: template `orig`, specialized for `vals`.
struct Job {
    orig: String,
    vals: Vec<CtValue>,
}

/// The monomorphization worklist and its results.
#[derive(Default)]
struct Mono {
    queue: VecDeque<Job>,
    /// Mangled names already requested (dedups identical instantiations).
    done: HashSet<String>,
    /// Generated specializations, by template name (in generation order).
    generated: HashMap<String, Vec<Stmt>>,
}

/// The specialized name for `orig` at value arguments `vals` — e.g. `f$0`, `f$1`.
/// `$` cannot appear in a source identifier, so a specialization never collides
/// with a user-written name.
fn mangle(orig: &str, vals: &[CtValue]) -> String {
    let mut s = orig.to_string();
    for v in vals {
        s.push('$');
        s.push_str(&v.to_string());
    }
    s
}

fn compare_ints(op: InfixOp, a: i64, b: i64) -> Result<bool, ComptimeError> {
    use InfixOp::*;
    Ok(match op {
        Eq => a == b,
        Ne => a != b,
        Lt => a < b,
        Gt => a > b,
        Le => a <= b,
        Ge => a >= b,
        _ => {
            return Err(ComptimeError::NotComptime(
                "not a comparison operator".to_string(),
            ));
        }
    })
}

fn mk(kind: StmtKind, span: Span) -> Stmt {
    Stmt {
        kind,
        span,
        module: None,
    }
}

fn lit_result(val: &CtValue, span: Span) -> Result<Expr, ComptimeError> {
    val.materialize(span).ok_or_else(|| {
        ComptimeError::NotComptime(
            "type-valued or symbolic comptime values cannot materialize at runtime".to_string(),
        )
    })
}

fn ct_to_vm(value: &CtValue) -> Result<Value, ComptimeError> {
    match value {
        CtValue::Int(n) => Ok(Value::Int(*n)),
        CtValue::Bool(b) => Ok(Value::Bool(*b)),
        CtValue::Str(s) => Ok(Value::Str(s.clone())),
        CtValue::Tuple(items) => Ok(Value::Tuple(
            items.iter().map(ct_to_vm).collect::<Result<Vec<_>, _>>()?,
        )),
        CtValue::List(items) => Ok(Value::List(
            items.iter().map(ct_to_vm).collect::<Result<Vec<_>, _>>()?,
        )),
        CtValue::Type(_) | CtValue::Param(_) => Err(ComptimeError::NotComptime(
            "type-valued or symbolic values cannot cross into VM CTFE".to_string(),
        )),
    }
}

fn vm_to_ct(value: Value) -> Result<CtValue, ComptimeError> {
    match value {
        Value::Int(n) => Ok(CtValue::Int(n)),
        Value::Bool(b) => Ok(CtValue::Bool(b)),
        Value::Str(s) => Ok(CtValue::Str(s)),
        Value::Tuple(items) => Ok(CtValue::Tuple(
            items
                .into_iter()
                .map(vm_to_ct)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Value::List(items) => Ok(CtValue::List(
            items
                .into_iter()
                .map(vm_to_ct)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Value::None => Err(ComptimeError::NotComptime(
            "VM CTFE function returned None; a compile-time value is required".to_string(),
        )),
        other => Err(ComptimeError::NotComptime(format!(
            "VM CTFE returned unsupported runtime value {other}"
        ))),
    }
}

fn vm_ctfe_safe_builtin(name: &str) -> bool {
    matches!(
        name,
        "range" | "abs" | "min" | "max" | "round" | "Int" | "UInt" | "Float64"
    )
}

fn scalar_type_name(name: &str) -> Option<Ty> {
    match name {
        "Int" => Some(Ty::Int),
        "UInt" => Some(Ty::UInt),
        "Bool" => Some(Ty::Bool),
        "String" => Some(Ty::String),
        "Float64" => Some(Ty::Float64),
        "None" => Some(Ty::None),
        _ => None,
    }
}

mod rewrite;
use rewrite::*;
