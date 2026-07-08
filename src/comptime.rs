//! Compile-time elaboration (Phase 4 — comptime semantics).
//!
//! A pass between parsing and type-checking that **resolves compile-time
//! constructs before runtime lowering**, per `comptime.md` / `zig-comptime.md`:
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
//! - **CTFE** — a `comptime` context may call a **pure top-level function**, executed
//!   by a small fuel-bounded AST interpreter (`next_power_of_two(17)`); no I/O, no
//!   runtime state. This is an intentionally small "CTFE island", not a second VM.
//! - **Materialization** — module-level `comptime` constants are inlined as literals
//!   into runtime code, so a top-level comptime value is usable inside functions.
//!
//! Compile-time values are `Int`/`Bool`/`String`/`Tuple`/`List` (the shared
//! [`CtValue`](crate::ct::CtValue)).

use crate::ast::{Expr, ExprKind, InfixOp, PrefixOp, Stmt, StmtKind, WithItem};
use crate::ct::CtValue;
use crate::token::Span;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

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

/// Non-local control flow while executing a comptime function body (CTFE).
enum CtFlow {
    Normal,
    Return(CtValue),
    Break,
    Continue,
}

/// A CTFE-callable function: a pure, non-generic, top-level `def`.
struct CtFn<'a> {
    params: Vec<String>,
    body: &'a [Stmt],
}

/// The compile-time elaboration engine: the CTFE-callable functions and a shared
/// fuel budget. `top_consts` captures module-level constants for materialization.
struct Elab<'a> {
    fns: HashMap<String, CtFn<'a>>,
    fuel: Cell<usize>,
    top_consts: RefCell<HashMap<String, CtValue>>,
}

/// Elaborate all compile-time constructs in a program, returning an ordinary AST.
pub fn elaborate(program: Vec<Stmt>) -> Result<Vec<Stmt>, ComptimeError> {
    let elab = Elab {
        fns: collect_fns(&program),
        fuel: Cell::new(FUEL),
        top_consts: RefCell::new(HashMap::new()),
    };
    let mut env = HashMap::new();
    let elaborated = elab.block(&program, &mut env, false)?;
    // Materialize module-level comptime constants into runtime literals.
    let consts = elab.top_consts.into_inner();
    Ok(materialize_block(elaborated, &consts))
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
            && type_params.is_empty()
        {
            fns.insert(
                name.clone(),
                CtFn {
                    params: params.iter().map(|p| p.name.clone()).collect(),
                    body,
                },
            );
        }
    }
    fns
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
            self.stmt(stmt, env, in_fn, &mut out)?;
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
                        value: lit(&v, span),
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
            ExprKind::Identifier(name) => scope
                .get(name)
                .cloned()
                .ok_or_else(|| ComptimeError::NotComptime(name.clone())),
            ExprKind::TupleLit(elems) => Ok(CtValue::Tuple(self.eval_all(elems, scope)?)),
            ExprKind::ListLit(elems) => Ok(CtValue::List(self.eval_all(elems, scope)?)),
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
            // A call into a pure top-level function → CTFE.
            ExprKind::Call { name, args, .. } => {
                let argv = self.eval_all(args, scope)?;
                self.ctfe_call(name, argv)
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

    fn ctfe_call(&self, name: &str, args: Vec<CtValue>) -> Result<CtValue, ComptimeError> {
        let f = self.fns.get(name).ok_or_else(|| {
            ComptimeError::NotComptime(format!("'{name}' is not a compile-time-callable function"))
        })?;
        if f.params.len() != args.len() {
            return Err(ComptimeError::Arity(format!(
                "'{name}' expects {} argument(s), got {}",
                f.params.len(),
                args.len()
            )));
        }
        self.burn()?;
        let mut locals: HashMap<String, CtValue> = f.params.iter().cloned().zip(args).collect();
        match self.exec_block(f.body, &mut locals)? {
            CtFlow::Return(v) => Ok(v),
            _ => Err(ComptimeError::NotComptime(format!(
                "compile-time call to '{name}' did not return a value"
            ))),
        }
    }

    fn exec_block(
        &self,
        stmts: &[Stmt],
        locals: &mut HashMap<String, CtValue>,
    ) -> Result<CtFlow, ComptimeError> {
        for s in stmts {
            match self.exec_stmt(s, locals)? {
                CtFlow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(CtFlow::Normal)
    }

    fn exec_stmt(
        &self,
        s: &Stmt,
        locals: &mut HashMap<String, CtValue>,
    ) -> Result<CtFlow, ComptimeError> {
        self.burn()?;
        match &s.kind {
            StmtKind::VarDecl { name, value, .. } | StmtKind::Assign { name, value } => {
                let v = self.eval(value, locals)?;
                locals.insert(name.clone(), v);
                Ok(CtFlow::Normal)
            }
            StmtKind::Return(Some(e)) => Ok(CtFlow::Return(self.eval(e, locals)?)),
            StmtKind::Return(None) => Err(ComptimeError::NotComptime(
                "a compile-time function must return a value".to_string(),
            )),
            StmtKind::If { branches, orelse } => {
                for (c, b) in branches {
                    if self.eval(c, locals)?.as_bool("if condition")? {
                        return self.exec_block(b, locals);
                    }
                }
                match orelse {
                    Some(b) => self.exec_block(b, locals),
                    None => Ok(CtFlow::Normal),
                }
            }
            StmtKind::While { cond, body } => {
                while self.eval(cond, locals)?.as_bool("while condition")? {
                    self.burn()?;
                    match self.exec_block(body, locals)? {
                        CtFlow::Normal | CtFlow::Continue => {}
                        CtFlow::Break => break,
                        ret @ CtFlow::Return(_) => return Ok(ret),
                    }
                }
                Ok(CtFlow::Normal)
            }
            StmtKind::For { var, iter, body } => {
                for v in self.eval_iter(iter, locals)? {
                    self.burn()?;
                    locals.insert(var.clone(), v);
                    match self.exec_block(body, locals)? {
                        CtFlow::Normal | CtFlow::Continue => {}
                        CtFlow::Break => break,
                        ret @ CtFlow::Return(_) => return Ok(ret),
                    }
                }
                Ok(CtFlow::Normal)
            }
            StmtKind::Break => Ok(CtFlow::Break),
            StmtKind::Continue => Ok(CtFlow::Continue),
            StmtKind::Pass => Ok(CtFlow::Normal),
            StmtKind::Expr(e) => {
                self.eval(e, locals)?;
                Ok(CtFlow::Normal)
            }
            _ => Err(ComptimeError::NotComptime(
                "statement not allowed in a compile-time function".to_string(),
            )),
        }
    }
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
    Stmt { kind, span }
}

/// Materialize a compile-time value as a literal expression. The elaborator never
/// produces a symbolic `Param`, so materialization always succeeds here.
fn lit(val: &CtValue, span: Span) -> Expr {
    val.materialize(span)
        .expect("comptime elaboration never yields a symbolic Param value")
}

// --- Substitution / materialization -----------------------------------------
//
// One generic rewrite over the AST parameterized by a name→value lookup, used for
// two things: substituting a `comptime for` loop variable with its literal (in the
// unrolled body — does NOT descend into nested `def`/`struct`), and materializing
// module-level `comptime` constants into runtime literals (does descend, minus a
// function's own parameter names, which shadow).

/// A name→compile-time-value lookup for a rewrite.
type Subs<'a> = &'a dyn Fn(&str) -> Option<CtValue>;

/// Materialize module-level comptime constants throughout a program.
fn materialize_block(stmts: Vec<Stmt>, consts: &HashMap<String, CtValue>) -> Vec<Stmt> {
    let subs: Subs = &|n| consts.get(n).cloned();
    stmts
        .into_iter()
        .map(|s| rewrite_stmt_cloned(&s, subs, true))
        .collect()
}

fn rewrite_stmt_cloned(s: &Stmt, subs: Subs, into_defs: bool) -> Stmt {
    let mut s = s.clone();
    rewrite_stmt(&mut s, subs, into_defs);
    s
}

fn rewrite_expr(e: &mut Expr, subs: Subs) {
    match &mut e.kind {
        ExprKind::Identifier(name) => {
            if let Some(v) = subs(name) {
                *e = lit(&v, e.span);
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
