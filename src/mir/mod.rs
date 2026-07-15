//! Stage 5: flatten the HIR's nested expressions into
//! **A-Normal Form** (three-address code) — every subexpression becomes a
//! `MirInstr` writing a fresh [`Reg`], so `foo(bar(x))` becomes
//! `t0 = bar(x); t1 = foo(t0)`. The flattened form is what Stages 6–7 dataflow
//! analysis (liveness / move / borrow) runs over, and what the backends consume.
//!
//! One current limitation, intentional at this stage:
//! * **Fields/methods are name-based.** There is no type/layout info here, so
//!   member access keeps the field *name*; the backend resolves it to an offset
//!   (Tier-2) or a `Vec` index (Tier-1 VM).
//!
//! Entry points: [`lower_cfg`] turns one HIR [`Cfg`] (a function body) into a
//! [`MirFunction`]; [`lower_program`] lowers a whole program to one
//! [`MirFunction`] per `def` / struct method plus a synthetic `__toplevel__`.
//!
//! Writes go through a **place** ([`MirPlace`] = a root variable + [`Proj`]
//! chain), mirroring `rustc` MIR's place/rvalue split, so `p.items[i].x = e` and
//! `xs[i] += e` lower uniformly (indices evaluated once).

use crate::ast::{
    ArgConvention, Dtype, Expr, ExprKind, FnParam, InfixOp, ParamArg, ParamKind, PrefixOp, Stmt,
    StmtKind, TStringPart, Type as SourceType,
};
use crate::call::{effective_keyword_only_index, regular_marker_index};
use crate::checked::AnnotationSite;
use crate::checked::{CheckedConst, CheckedProgram};
use crate::hir::{self, Cfg, HirInstr, Terminator, VarId};
use crate::token::{DUMMY_SPAN, SourceSpan};
use crate::types::Ty;
use std::collections::{HashMap, HashSet};

mod ir;
pub use ir::*;

/// An expression's source span, stamped by the parser (`ast::Expr.span`). Fed
/// into the [`SpanTable`] so each temporary can be traced back to its origin.
fn span(e: &Expr) -> SourceSpan {
    e.source_span()
}

/// Resolve a `SIMD` dtype parameter argument (`DType.<name>`) to a [`Dtype`].
fn dtype_from_param_arg(arg: &ParamArg) -> Option<Dtype> {
    if let ParamArg::Value(Expr {
        kind: ExprKind::Member { object, field },
        ..
    }) = arg
        && let ExprKind::Identifier(ns) = &object.kind
        && ns == "DType"
    {
        return Dtype::from_name(field);
    }
    None
}

/// Const-fold a compile-time `Int` parameter argument (a SIMD width). Handles the
/// literal / unary-minus / arithmetic forms; the checker has already validated it.
fn ct_int(arg: &ParamArg) -> Option<i64> {
    match arg {
        ParamArg::Value(e) => ct_int_expr(e),
        ParamArg::Type(_) => None,
    }
}

fn ct_int_expr(e: &Expr) -> Option<i64> {
    match &e.kind {
        ExprKind::Int(n) => Some(*n),
        ExprKind::Prefix(PrefixOp::Neg, inner) => Some(-ct_int_expr(inner)?),
        ExprKind::Infix(op, a, b) => {
            let (x, y) = (ct_int_expr(a)?, ct_int_expr(b)?);
            match op {
                InfixOp::Add => Some(x + y),
                InfixOp::Sub => Some(x - y),
                InfixOp::Mul => Some(x * y),
                InfixOp::FloorDiv if y != 0 => Some(x.div_euclid(y)),
                InfixOp::Mod if y != 0 => Some(x.rem_euclid(y)),
                InfixOp::Pow if y >= 0 => Some(x.pow(y as u32)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// A nested `def` lifted to a top-level function: the mangled name it becomes and
/// the enclosing locals it captures. Captures are passed as leading **`mut`**
/// parameters (so a read *or* a write of a captured variable works — reference
/// semantics via the existing write-back), prepended to a call by name.
#[derive(Clone)]
struct NestedInfo {
    mangled: String,
    /// Captured enclosing-local names, in a deterministic (sorted) order shared by
    /// the lifted function's parameter list and every rewritten call site.
    captures: Vec<String>,
}

/// Flattens nested `Expr`s into a block's instruction list. `cur` is the block
/// currently being appended to.
struct Flatten<'a> {
    f: &'a mut MirFunction,
    cur: MirBlockId,
    next_reg: u32,
    /// Interner: a variable name's first appearance assigns its `VarId`.
    vars: Vec<String>,
    /// Nested `def`s in scope (name → lifted target + captures); a call to one is
    /// rewritten to the mangled function with its captures prepended, and the
    /// nested `def` statement itself lowers to nothing.
    nested: HashMap<String, NestedInfo>,
    /// The program's overloaded declarations. Kept as a fallback for
    /// unannotated lowering; normally overload_targets gives the exact callee.
    overloads: crate::symbol::OverloadSets,
    /// Checker-selected lowered callee names for overloaded call expressions.
    overload_targets: HashMap<SourceSpan, String>,
    /// Local reference slot to its frozen owner place and permission.
    aliases: HashMap<VarId, (MirPlace, bool)>,
    runtime_aliases: std::collections::HashSet<VarId>,
    returns_reference: bool,
}

impl Flatten<'_> {
    fn fresh(&mut self, span: SourceSpan, origin: Option<VarId>) -> Reg {
        let r = self.next_reg;
        self.next_reg += 1;
        self.f.spans.0.insert(r, (span, origin));
        Reg(r)
    }

    fn emit(&mut self, i: MirInstr) {
        self.f.blocks[self.cur].instrs.push(i);
    }

    /// The callee for a free call the checker recorded no exact target for: the
    /// plain name when it isn't overloaded (the common case), else a poison
    /// marker — an overloaded call off the checked path must not guess.
    fn overloaded_name(&self, name: &str, argc: usize) -> String {
        if self.overloads.function_is_overloaded(name, argc) {
            crate::symbol::unresolved_overload_marker(name, argc)
        } else {
            name.to_string()
        }
    }

    /// Intern a variable name to a stable `VarId` (matches `hir::Lower::var`).
    fn var(&mut self, name: &str) -> VarId {
        if let Some(i) = self.vars.iter().position(|n| n == name) {
            i as VarId
        } else {
            self.vars.push(name.to_string());
            (self.vars.len() - 1) as VarId
        }
    }

    fn resolved_place(&mut self, name: &str) -> MirPlace {
        let var = self.var(name);
        self.aliases
            .get(&var)
            .map(|(place, _)| {
                let mut place = place.clone();
                place.through = Some(var);
                place
            })
            .unwrap_or(MirPlace {
                root: var,
                proj: Vec::new(),
                through: None,
            })
    }

    /// Flatten one or more argument expressions to their result registers.
    fn args(&mut self, args: &[Expr]) -> Vec<Reg> {
        args.iter().map(|a| self.expr(a)).collect()
    }

    /// Intern a fresh synthetic variable (a `$`-prefixed name never produced by
    /// the parser), used to carry a short-circuit result across CFG blocks.
    fn fresh_var(&mut self) -> VarId {
        let id = self.vars.len();
        self.vars.push(format!("$sc{id}"));
        id as VarId
    }

    /// Append a new empty basic block (placeholder terminator) and return its id.
    fn new_block(&mut self) -> MirBlockId {
        self.f.blocks.push(MirBlock {
            instrs: Vec::new(),
            term: MirTerm::Return(None),
        });
        self.f.blocks.len() - 1
    }

    /// Lower `a and b` / `a or b` into control flow so the right operand is only
    /// evaluated when needed (Python/Mojo short-circuit semantics). The result is
    /// carried in a synthetic variable across the branch and read back in the
    /// merge block. (Preserving the short-circuit — vs an eager `BinOp` — matters
    /// both for observable side effects and for Stage 6 ownership, where a moved
    /// operand on the not-taken side must not count as moved.)
    fn short_circuit(&mut self, op: InfixOp, a: &Expr, b: &Expr, span: SourceSpan) -> Reg {
        let ra = self.expr(a);
        let result = self.fresh_var();
        // Seed the result with the left operand's value: for `and` a false `ra`
        // is the answer; for `or` a true `ra` is. The rhs block overwrites it.
        self.emit(MirInstr::DefVar {
            var: result,
            src: ra,
            source_annotation: None,
        });

        let rhs_blk = self.new_block();
        let merge_blk = self.new_block();
        // `and`: evaluate rhs only when `ra` is true; `or`: only when false.
        let (then_b, else_b) = match op {
            InfixOp::And => (rhs_blk, merge_blk),
            _ => (merge_blk, rhs_blk),
        };
        self.f.blocks[self.cur].term = MirTerm::Branch {
            cond: ra,
            then_b,
            else_b,
        };

        self.cur = rhs_blk;
        let rb = self.expr(b); // may itself split blocks (nested and/or)
        self.emit(MirInstr::DefVar {
            var: result,
            src: rb,
            source_annotation: None,
        });
        self.f.blocks[self.cur].term = MirTerm::Jump(merge_blk);

        self.cur = merge_blk;
        let d = self.fresh(span, None);
        self.emit(MirInstr::UseVar {
            dest: d,
            var: result,
            mode: UseMode::Copy,
        });
        d
    }

    /// Lower a ternary `then_e if cond else else_e` to a value: branch on `cond`,
    /// each arm writing the result variable, then read it at the merge.
    fn ternary(&mut self, cond: &Expr, then_e: &Expr, else_e: &Expr, sp: SourceSpan) -> Reg {
        let rc = self.expr(cond);
        let result = self.fresh_var();
        let then_blk = self.new_block();
        let else_blk = self.new_block();
        let merge_blk = self.new_block();
        self.f.blocks[self.cur].term = MirTerm::Branch {
            cond: rc,
            then_b: then_blk,
            else_b: else_blk,
        };
        self.cur = then_blk;
        let rt = self.expr(then_e);
        self.emit(MirInstr::DefVar {
            var: result,
            src: rt,
            source_annotation: None,
        });
        self.f.blocks[self.cur].term = MirTerm::Jump(merge_blk);
        self.cur = else_blk;
        let re = self.expr(else_e);
        self.emit(MirInstr::DefVar {
            var: result,
            src: re,
            source_annotation: None,
        });
        self.f.blocks[self.cur].term = MirTerm::Jump(merge_blk);
        self.cur = merge_blk;
        let d = self.fresh(sp, None);
        self.emit(MirInstr::UseVar {
            dest: d,
            var: result,
            mode: UseMode::Copy,
        });
        d
    }

    /// Lower a chained comparison `a op1 b op2 c …` to a `Bool`. Each operand is
    /// evaluated **once**, left to right; a false link short-circuits the rest (the
    /// remaining operands are not evaluated). The result variable holds the last
    /// comparison evaluated (which is `false` on the link that failed).
    fn compare_chain(&mut self, first: &Expr, rest: &[(InfixOp, Expr)], sp: SourceSpan) -> Reg {
        let result = self.fresh_var();
        let merge_blk = self.new_block();
        let mut prev = self.expr(first);
        for (i, (op, operand)) in rest.iter().enumerate() {
            let cur = self.expr(operand);
            let cmp = self.fresh(sp.clone(), None);
            self.emit(MirInstr::BinOp {
                op: *op,
                dest: cmp,
                a: prev,
                b: cur,
            });
            self.emit(MirInstr::DefVar {
                var: result,
                src: cmp,
                source_annotation: None,
            });
            if i + 1 == rest.len() {
                self.f.blocks[self.cur].term = MirTerm::Jump(merge_blk);
            } else {
                // A false link is the answer (result is already it); a true link
                // continues to the next comparison.
                let next_blk = self.new_block();
                self.f.blocks[self.cur].term = MirTerm::Branch {
                    cond: cmp,
                    then_b: next_blk,
                    else_b: merge_blk,
                };
                self.cur = next_blk;
                prev = cur;
            }
        }
        self.cur = merge_blk;
        let d = self.fresh(sp, None);
        self.emit(MirInstr::UseVar {
            dest: d,
            var: result,
            mode: UseMode::Copy,
        });
        d
    }

    /// Post-order: each subexpression emits one instruction and yields its result
    /// `Reg`, so `foo(bar(x))` → `t0 = bar(x); t1 = foo(t0)`. Total over `Expr`.
    fn expr(&mut self, e: &Expr) -> Reg {
        match &e.kind {
            // --- Literals ------------------------------------------------------
            ExprKind::Int(n) => self.constant(e, Const::Int(*n)),
            ExprKind::Float(x) => self.constant(e, Const::Float(*x)),
            ExprKind::Bool(b) => self.constant(e, Const::Bool(*b)),
            ExprKind::Str(s) => self.constant(e, Const::Str(s.clone())),
            ExprKind::None => self.constant(e, Const::None),

            // --- Variable reads ------------------------------------------------
            // A bare read defaults to `Copy`; a call site refines it to
            // `Borrow*`/`Move` per the callee's convention (Stage 6).
            ExprKind::Identifier(name) => {
                let var = self.var(name);
                let d = self.fresh(span(e), Some(var));
                if let Some((mut place, _)) = self.aliases.get(&var).cloned() {
                    place.through = Some(var);
                    self.emit(MirInstr::LoadPlace { dest: d, place });
                } else {
                    self.emit(MirInstr::UseVar {
                        dest: d,
                        var,
                        mode: UseMode::Copy,
                    });
                }
                d
            }
            // `x^`: a move out of a variable. `p.a^` (a pure field chain) is a
            // **partial move** of that field; a move through an indexed place is
            // identity for now (conservative — Stage 6 does not model it).
            ExprKind::Transfer(inner) => {
                if let ExprKind::Identifier(name) = &inner.kind {
                    let var = self.var(name);
                    let d = self.fresh(span(e), Some(var));
                    self.emit(MirInstr::UseVar {
                        dest: d,
                        var,
                        mode: UseMode::Move,
                    });
                    d
                } else if let Some(place) = self.pure_field_place(inner) {
                    let d = self.fresh(span(e), Some(place.root));
                    self.emit(MirInstr::MovePlace { dest: d, place });
                    d
                } else {
                    self.expr(inner)
                }
            }

            // --- Operators -----------------------------------------------------
            ExprKind::Prefix(op, a) => {
                let ra = self.expr(a);
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::UnOp {
                    op: *op,
                    dest: d,
                    a: ra,
                });
                d
            }
            // `and`/`or` short-circuit — lowered to CFG blocks, not an eager BinOp.
            ExprKind::Infix(op @ (InfixOp::And | InfixOp::Or), a, b) => {
                self.short_circuit(*op, a, b, span(e))
            }
            ExprKind::Infix(op, a, b) => {
                let ra = self.expr(a); // operands left-to-right (evaluation order is explicit)
                let rb = self.expr(b);
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::BinOp {
                    op: *op,
                    dest: d,
                    a: ra,
                    b: rb,
                });
                d
            }

            // --- Calls / access ------------------------------------------------
            // NOTE: keyword args + default-slot matching (`call::match_call_slots`)
            // are a follow-up; the checker has already validated them, so only the
            // positional `args` are flattened here.
            ExprKind::Call {
                name,
                param_args,
                args,
                kwargs,
            } => {
                // SIMD construction resolves its `[DType.<dt>, width]` parameters
                // here (the MIR is otherwise untyped about them).
                if let Some(r) = self.try_simd_call(e, name, param_args, args) {
                    return r;
                }
                // A call to a nested `def` (a closure, called by name in scope):
                // rewrite to its lifted function, prepending the captured enclosing
                // locals as leading arguments (passed as places, so the `mut`
                // capture parameters write back — reference-capture semantics).
                if let Some(info) = self.nested.get(name).cloned() {
                    return self.lower_nested_call(e, &info, args);
                }
                // Compile-time parameter arguments (`Name[param_args](...)`),
                // evaluated before ordinary call arguments: a
                // **value** parameter is a comptime `Int` expression flattened to a
                // register; a **type** parameter is erased (`None`).
                let param_arg_regs: Vec<Option<Reg>> = param_args
                    .iter()
                    .map(|pa| match pa {
                        ParamArg::Value(e) => Some(self.expr(e)),
                        ParamArg::Type(_) => None,
                    })
                    .collect();
                let regs = self.args(args);
                // Capture each positional argument's place (if it is a simple one),
                // so the VM can write back a `mut`/`ref` parameter after the call.
                let arg_places: Vec<Option<MirPlace>> =
                    args.iter().map(|a| self.simple_place(a)).collect();
                let kw: Vec<(String, Reg)> = kwargs
                    .iter()
                    .map(|k| (k.name.clone(), self.expr(&k.value)))
                    .collect();
                let target = self
                    .overload_targets
                    .get(&span(e))
                    .cloned()
                    .unwrap_or_else(|| self.overloaded_name(name, args.len()));
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::Call {
                    dest: d,
                    func: FuncRef::named(&target),
                    args: regs,
                    kwargs: kw,
                    arg_places,
                    param_arg_regs,
                });
                d
            }
            ExprKind::MethodCall {
                object,
                method,
                args,
                kwargs,
            } => {
                // A **static** method on a parameterized built-in type — the receiver
                // is a type, not a value (`UnsafePointer[T].alloc(n)`). Lower to a
                // builtin call `Type.method(args)`; the element type is erased.
                if let ExprKind::TypeApply { name, .. } = &object.kind {
                    let regs = self.args(args);
                    let kw: Vec<(String, Reg)> = kwargs
                        .iter()
                        .map(|k| (k.name.clone(), self.expr(&k.value)))
                        .collect();
                    let d = self.fresh(span(e), None);
                    self.emit(MirInstr::Call {
                        dest: d,
                        func: FuncRef::named(&format!("{name}.{method}")),
                        args: regs,
                        kwargs: kw,
                        arg_places: vec![None; args.len()],
                        param_arg_regs: Vec::new(),
                    });
                    return d;
                }
                // If the receiver is a place, load it through that place (indices
                // evaluated once) and keep the place for write-back; otherwise it is
                // a temporary evaluated for its value only.
                let (recv, recv_place) = match self.try_place(object) {
                    Some(place) => {
                        let recv = self.fresh(span(e), None);
                        self.emit(MirInstr::LoadPlace {
                            dest: recv,
                            place: place.clone(),
                        });
                        (recv, Some(place))
                    }
                    None => (self.expr(object), None),
                };
                let regs = self.args(args);
                let kw: Vec<(String, Reg)> = kwargs
                    .iter()
                    .map(|arg| (arg.name.clone(), self.expr(&arg.value)))
                    .collect();
                // Capture each ordinary argument's place (if simple) for `mut`/`ref`
                // ordinary-parameter write-back, mirroring a free-function `Call`.
                let arg_places: Vec<Option<MirPlace>> =
                    args.iter().map(|a| self.simple_place(a)).collect();
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::MethodCall {
                    dest: d,
                    recv,
                    method: method.clone(),
                    resolved: self.overload_targets.get(&span(e)).cloned(),
                    args: regs,
                    kwargs: kw,
                    recv_place,
                    arg_places,
                });
                d
            }
            ExprKind::Member { object, field } => {
                // A pure field chain rooted at a variable (`p.a`, `p.a.b`) lowers to
                // a `LoadPlace` (a place read) so the ownership analysis sees *which*
                // field is read — enabling field-sensitive partial-move checking
                // (reading `p.b` after `p.a^` stays legal). A member of a temporary
                // or an indexed base keeps the register-based `GetField`.
                if let Some(place) = self.pure_field_place(e) {
                    let d = self.fresh(span(e), Some(place.root));
                    self.emit(MirInstr::LoadPlace { dest: d, place });
                    d
                } else {
                    let base = self.expr(object);
                    let d = self.fresh(span(e), None);
                    self.emit(MirInstr::GetField {
                        dest: d,
                        base,
                        field: field.clone(),
                    });
                    d
                }
            }
            ExprKind::Index { object, index } => {
                let base = self.expr(object);
                let idx = self.expr(index);
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::Index {
                    dest: d,
                    base,
                    index: idx,
                });
                d
            }

            // --- Aggregates ----------------------------------------------------
            ExprKind::ListLit(elems) => {
                let regs = self.args(elems);
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::MakeList {
                    dest: d,
                    elems: regs,
                });
                d
            }
            ExprKind::TupleLit(elems) => {
                let regs = self.args(elems);
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::MakeTuple {
                    dest: d,
                    elems: regs,
                });
                d
            }

            // Walrus `:=` reaches MIR after type checking. Preserve an explicit
            // unsupported boundary rather than assigning accidental semantics.
            ExprKind::Named { .. } => {
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::Unsupported("the walrus operator ':='".into()));
                d
            }
            // Ternary `a if cond else b` — a value-producing branch (like the
            // short-circuit lowering, but both arms assign the result).
            ExprKind::IfExpr {
                cond,
                then_branch,
                else_branch,
            } => self.ternary(cond, then_branch, else_branch, span(e)),
            // Chained comparison `a < b < c` — each operand evaluated once, folded
            // into short-circuiting `and`s.
            ExprKind::Compare { first, rest } => self.compare_chain(first, rest, span(e)),
            // Slice `object[lower:upper:step]` → a new List/String.
            ExprKind::Slice {
                object,
                lower,
                upper,
                step,
            } => {
                let obj = self.expr(object);
                let lower = lower.as_ref().map(|b| self.expr(b));
                let upper = upper.as_ref().map(|b| self.expr(b));
                let step = step.as_ref().map(|b| self.expr(b));
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::Slice {
                    dest: d,
                    object: obj,
                    lower,
                    upper,
                    step,
                });
                d
            }
            // These are flagged `Unsupported`/rejected by the *checker*, so a checked
            // program never reaches MIR lowering with them. A bare `TypeApply` is a
            // type used as a value (only valid as a static-method receiver, handled
            // in the `MethodCall` arm above).
            ExprKind::TypeValue(_)
            | ExprKind::Invoke { .. }
            | ExprKind::BraceLit(_)
            | ExprKind::TypeApply { .. }
            | ExprKind::TString { .. } => {
                let dest = self.fresh(span(e), None);
                self.emit(MirInstr::Unsupported(format!(
                    "unchecked expression reached MIR lowering: {:?}",
                    e.kind
                )));
                self.emit(MirInstr::Const {
                    dest,
                    k: Const::None,
                });
                dest
            }
        }
    }

    /// If `name(...)` is a SIMD construction — `SIMD[DType.<dt>, width](elems)` or
    /// a scalar alias (`Int32(x)`, `Float32(x)`, …) — resolve its dtype/width and
    /// emit a [`MirInstr::MakeSimd`], returning its result register. Otherwise
    /// `None`, and the caller lowers it as an ordinary call.
    fn try_simd_call(
        &mut self,
        e: &Expr,
        name: &str,
        param_args: &[ParamArg],
        args: &[Expr],
    ) -> Option<Reg> {
        let (dtype, width) = if name == "SIMD" {
            let dtype = dtype_from_param_arg(param_args.first()?)?;
            let width = ct_int(param_args.get(1)?)? as usize;
            (dtype, width)
        } else {
            (Dtype::from_scalar_alias(name)?, 1)
        };
        let elems = self.args(args);
        let d = self.fresh(span(e), None);
        self.emit(MirInstr::MakeSimd {
            dest: d,
            dtype,
            width,
            elems,
        });
        Some(d)
    }

    /// Lower a call to a nested `def` (`info`), prepending the captured enclosing
    /// locals as leading arguments — each read as a value **and** kept as a place,
    /// so the lifted function's `mut` capture parameters write back (reference
    /// semantics). The declared `args` follow. Captures come first so they align
    /// with the lifted function's parameter order.
    fn lower_nested_call(&mut self, e: &Expr, info: &NestedInfo, args: &[Expr]) -> Reg {
        let mut arg_regs: Vec<Reg> = Vec::new();
        let mut arg_places: Vec<Option<MirPlace>> = Vec::new();
        for cap in &info.captures {
            let var = self.var(cap);
            let r = self.fresh(span(e), Some(var));
            self.emit(MirInstr::UseVar {
                dest: r,
                var,
                mode: UseMode::Copy,
            });
            arg_regs.push(r);
            arg_places.push(Some(MirPlace {
                root: var,
                proj: Vec::new(),
                through: None,
            }));
        }
        for a in args {
            arg_regs.push(self.expr(a));
            arg_places.push(self.simple_place(a));
        }
        let d = self.fresh(span(e), None);
        self.emit(MirInstr::Call {
            dest: d,
            func: FuncRef::named(&info.mangled),
            args: arg_regs,
            kwargs: Vec::new(),
            arg_places,
            param_arg_regs: Vec::new(),
        });
        d
    }

    /// Emit a `Const` writing a fresh register.
    fn constant(&mut self, e: &Expr, k: Const) -> Reg {
        let d = self.fresh(span(e), None);
        self.emit(MirInstr::Const { dest: d, k });
        d
    }

    // --- The driver's per-instruction / per-terminator lowering -----------------

    /// Lower one straight-line HIR instruction into `self.cur`. `outer_map` is the
    /// enclosing **function**'s HIR→MIR block map, used to resolve a `try`'s
    /// escape targets (`break`/`continue` to an outer loop); most arms ignore it.
    fn lower_instr(&mut self, i: &HirInstr, outer_map: &HashMap<hir::BlockId, MirBlockId>) {
        match i {
            HirInstr::Bind {
                dest,
                expr,
                source_annotation,
            } => {
                let src = self.expr(expr);
                if let Some((mut place, _)) = self.aliases.get(dest).cloned() {
                    place.through = Some(*dest);
                    self.emit(MirInstr::Store { place, src });
                } else if self.runtime_aliases.contains(dest) {
                    let handle = self.fresh(expr.source_span(), Some(*dest));
                    self.emit(MirInstr::MakeRef {
                        dest: handle,
                        place: MirPlace {
                            root: *dest,
                            proj: Vec::new(),
                            through: Some(*dest),
                        },
                    });
                    self.emit(MirInstr::WriteRef {
                        reference: handle,
                        value: src,
                    });
                } else {
                    self.emit(MirInstr::DefVar {
                        var: *dest,
                        src,
                        source_annotation: source_annotation.clone(),
                    });
                }
            }
            HirInstr::Eval(e) => {
                let _ = self.expr(e); // evaluated for its effect; result discarded
            }
            HirInstr::Stmt(s) => self.lower_stmt(s, outer_map),
            // A `try` whose enclosing loops are function-level: lower each sub-region
            // seeded with those loops (`loop_targets`, HIR function block ids), so an
            // outward `break`/`continue` becomes an `EscapeJump` resolved via
            // `outer_map`.
            HirInstr::Try { stmt, loop_targets } => {
                if let StmtKind::Try {
                    body,
                    except,
                    orelse,
                    finalbody,
                } = &stmt.kind
                {
                    self.emit_try(body, except, orelse, finalbody, loop_targets, outer_map);
                } else {
                    self.emit(MirInstr::Unsupported(
                        "malformed HIR try instruction".to_string(),
                    ));
                }
            }
            HirInstr::Drop(var) => {
                self.emit(MirInstr::DropVar { var: *var });
            }
            // Iterator protocol: compute into a register, then store to the target
            // variable (so the header's branch can read `has_next` as a `UseVar`,
            // and the body binds the loop variable).
            HirInstr::GetIter { iter } => {
                self.emit(MirInstr::GetIter { iter: *iter });
            }
            HirInstr::HasNext { iter, dest } => {
                let r = self.fresh(SourceSpan::new(None, DUMMY_SPAN), None);
                self.emit(MirInstr::HasNext {
                    dest: r,
                    iter: *iter,
                });
                self.emit(MirInstr::DefVar {
                    var: *dest,
                    src: r,
                    source_annotation: None,
                });
            }
            HirInstr::Next { iter, dest } => {
                let r = self.fresh(SourceSpan::new(None, DUMMY_SPAN), Some(*iter));
                self.emit(MirInstr::Next {
                    dest: r,
                    iter: *iter,
                });
                self.emit(MirInstr::DefVar {
                    var: *dest,
                    src: r,
                    source_annotation: None,
                });
            }
        }
    }

    /// Decompose a place expression (`x`, `p.a.b`, `xs[i]`, `p.items[i].x`) into a
    /// [`MirPlace`] — a root variable plus a projection chain — flattening any
    /// subscript index into a register **once**. The checker guarantees the root
    /// is a variable (or `self`), so a non-variable root is unreachable.
    fn place(&mut self, e: &Expr) -> MirPlace {
        match &e.kind {
            ExprKind::Identifier(name) => self.resolved_place(name),
            ExprKind::Member { object, field } => {
                let mut p = self.place(object);
                p.proj.push(Proj::Field(field.clone()));
                p
            }
            ExprKind::Index { object, index } => {
                let mut p = self.place(object);
                let idx = self.expr(index); // evaluated once, before the store
                p.proj.push(Proj::Index(idx));
                p
            }
            other => {
                self.emit(MirInstr::Unsupported(format!(
                    "invalid assignment place reached MIR lowering: {other:?}"
                )));
                MirPlace {
                    root: self.var("$invalid_place"),
                    proj: Vec::new(),
                    through: None,
                }
            }
        }
    }

    /// Like [`place`](Self::place), but returns `None` for a non-place expression
    /// (a call result, a literal, …) instead of panicking — used at a method-call
    /// receiver, which may be a temporary. Only evaluates subscript indices when
    /// the whole chain is a place.
    /// Lower a `try` sub-region (`body`/`except`/`else`/`finally`) into a
    /// self-contained mini-CFG (block ids local, entry = 0) that **shares this
    /// function's register, variable, and span space** — so it addresses the same
    /// slots. The region's own control flow (`if`/`while`/`for`) becomes local
    /// blocks; the VM runs it recursively.
    fn lower_region(
        &mut self,
        body: &[Stmt],
        ext_loops: &[(hir::BlockId, hir::BlockId)],
        outer_map: &HashMap<hir::BlockId, MirBlockId>,
    ) -> Vec<MirBlock> {
        let region_cfg = hir::Cfg::build_seeded_with_loops(self.vars.clone(), body, ext_loops);
        let mut region = MirFunction {
            blocks: Vec::new(),
            n_regs: 0,
            n_vars: 0,
            var_names: Vec::new(),
            n_params: 0,
            param_annotations: Vec::new(),
            owned_params: Vec::new(),
            ref_params: Vec::new(),
            returns_reference: self.returns_reference,
            spans: std::mem::take(&mut self.f.spans), // accumulate into the shared table
        };
        let mut map: HashMap<hir::BlockId, MirBlockId> = HashMap::new();
        for hb in region_cfg.g.node_indices() {
            map.insert(hb, region.blocks.len());
            region.blocks.push(MirBlock {
                instrs: Vec::new(),
                term: MirTerm::Return(None),
            });
        }
        {
            let mut fl = Flatten {
                f: &mut region,
                cur: 0,
                next_reg: self.next_reg,
                vars: region_cfg.vars.clone(),
                nested: self.nested.clone(), // a `try` region may call a nested `def`
                overloads: self.overloads.clone(),
                overload_targets: self.overload_targets.clone(),
                aliases: self.aliases.clone(),
                runtime_aliases: self.runtime_aliases.clone(),
                returns_reference: self.returns_reference,
            };
            for hb in region_cfg.g.node_indices() {
                fl.cur = map[&hb];
                for instr in &region_cfg.g[hb].instrs {
                    fl.lower_instr(instr, outer_map);
                }
                let fallback = Terminator::FallOff;
                let term = region_cfg.g[hb].term.as_ref().unwrap_or(&fallback);
                // Region terminators resolve local jumps via the region's own `map`;
                // an `EscapeJump` resolves its outer-loop target via `outer_map`.
                let mterm = fl.lower_term(term, &map, outer_map);
                fl.f.blocks[fl.cur].term = mterm;
            }
            self.next_reg = fl.next_reg;
            self.vars = fl.vars.clone();
        }
        self.f.spans = std::mem::take(&mut region.spans);
        region.blocks
    }

    /// Lower a `try`'s sub-regions and emit the [`MirInstr::Try`]. `ext_loops` are
    /// the enclosing function loops (HIR block ids) a `break`/`continue` may escape
    /// to; `outer_map` resolves them to MIR blocks. Shared by the primary
    /// (`HirInstr::Try`) and fallback (`lower_stmt`) paths.
    #[allow(clippy::type_complexity)]
    fn emit_try(
        &mut self,
        body: &[Stmt],
        except: &Option<(Option<String>, Vec<Stmt>)>,
        orelse: &Option<Vec<Stmt>>,
        finalbody: &Option<Vec<Stmt>>,
        ext_loops: &[(hir::BlockId, hir::BlockId)],
        outer_map: &HashMap<hir::BlockId, MirBlockId>,
    ) {
        let body_blocks = self.lower_region(body, ext_loops, outer_map);
        let handler = match except {
            Some((name, ex_body)) => {
                let slot = name.as_ref().map(|n| self.var(n));
                let blocks = self.lower_region(ex_body, ext_loops, outer_map);
                Some((slot, blocks))
            }
            None => None,
        };
        let orelse_blocks = orelse
            .as_ref()
            .map(|b| self.lower_region(b, ext_loops, outer_map));
        let finalbody_blocks = finalbody
            .as_ref()
            .map(|b| self.lower_region(b, ext_loops, outer_map));
        self.emit(MirInstr::Try {
            body: body_blocks,
            handler,
            orelse: orelse_blocks,
            finalbody: finalbody_blocks,
            cleanup: Vec::new(),
        });
    }

    /// A place for a call argument's *write-back* — a variable or a field chain,
    /// **without** any dynamic index (so building it emits nothing and avoids
    /// re-evaluating an index that the argument's value already consumed). Returns
    /// `None` for a temporary or an indexed place (write-back to those is refused by
    /// the VM). Distinct from [`Self::try_place`], which emits index evaluations.
    fn simple_place(&mut self, e: &Expr) -> Option<MirPlace> {
        match &e.kind {
            ExprKind::Identifier(name) => Some(self.resolved_place(name)),
            ExprKind::Member { object, field } => {
                let mut p = self.simple_place(object)?;
                p.proj.push(Proj::Field(field.clone()));
                Some(p)
            }
            _ => None,
        }
    }

    /// Decompose `e` into a place **iff** it is a variable or a *pure field
    /// chain* rooted at one (`x`, `p.a`, `p.a.b`) — no dynamic index. Used to
    /// distinguish a place read (`LoadPlace`) from a temporary/indexed read, and
    /// a partial move (`p.a^`) from an untracked indexed transfer. Emits nothing.
    fn pure_field_place(&mut self, e: &Expr) -> Option<MirPlace> {
        match &e.kind {
            ExprKind::Identifier(name) => {
                // `Self.<name>` (a reified value-parameter read, e.g. `Self.size`)
                // resolves off the receiver `self`: `Self` in expression position is
                // an alias for `self`, and the backend's field navigation also
                // searches a struct's `value_params`. `Self` never appears bare in an
                // expression (only `Self.field`), so this alias is safe.
                let root = if name == "Self" { "self" } else { name };
                Some(self.resolved_place(root))
            }
            ExprKind::Member { object, field } => {
                let mut p = self.pure_field_place(object)?;
                p.proj.push(Proj::Field(field.clone()));
                Some(p)
            }
            _ => None,
        }
    }

    fn try_place(&mut self, e: &Expr) -> Option<MirPlace> {
        match &e.kind {
            ExprKind::Identifier(name) => Some(self.resolved_place(name)),
            ExprKind::Member { object, field } => {
                let mut p = self.try_place(object)?;
                p.proj.push(Proj::Field(field.clone()));
                Some(p)
            }
            ExprKind::Index { object, index } => {
                let mut p = self.try_place(object)?;
                let idx = self.expr(index);
                p.proj.push(Proj::Index(idx));
                Some(p)
            }
            _ => None,
        }
    }

    /// Lower the "catch-all" straight-line statements. Every reachable case is
    /// handled; the categorization decisions are documented per arm. `outer_map`
    /// threads the enclosing function's block map for a fallback-path `try`.
    fn lower_stmt(&mut self, s: &Stmt, outer_map: &HashMap<hir::BlockId, MirBlockId>) {
        match &s.kind {
            StmtKind::RefDecl { name, value } => {
                let reference = self.var(name);
                if !matches!(
                    value.kind,
                    ExprKind::Identifier(_) | ExprKind::Member { .. } | ExprKind::Index { .. }
                ) {
                    let source = self.expr(value);
                    self.runtime_aliases.insert(reference);
                    self.emit(MirInstr::DefVar {
                        var: reference,
                        src: source,
                        source_annotation: None,
                    });
                    let candidates: Vec<&Expr> = match &value.kind {
                        ExprKind::Call { args, kwargs, .. } => args
                            .iter()
                            .chain(kwargs.iter().map(|argument| &argument.value))
                            .collect(),
                        ExprKind::MethodCall {
                            object,
                            args,
                            kwargs,
                            ..
                        } => std::iter::once(object.as_ref())
                            .chain(args.iter())
                            .chain(kwargs.iter().map(|argument| &argument.value))
                            .collect(),
                        _ => Vec::new(),
                    };
                    for candidate in candidates {
                        if let Some(place) = self.simple_place(candidate) {
                            let marker = self.fresh(candidate.source_span(), Some(place.root));
                            self.emit(MirInstr::BeginLoan {
                                reference,
                                place,
                                mutable: true,
                                marker,
                            });
                        }
                    }
                    return;
                }
                let place = self.place(value);
                let mutable = true;
                self.aliases.insert(reference, (place.clone(), mutable));
                let marker = self.fresh(s.source_span(), Some(place.root));
                self.emit(MirInstr::BeginLoan {
                    reference,
                    place,
                    mutable,
                    marker,
                });
            }
            // --- Writes through a place (any nesting) --------------------------
            StmtKind::SetPlace { place, value } => {
                let src = self.expr(value);
                let p = self.place(place);
                self.emit(MirInstr::Store { place: p, src });
            }
            StmtKind::AugAssign { place, op, value } => {
                // `place OP= e` — read the place, apply the op, write it back. A bare
                // variable uses the `UseVar`/`DefVar` fast path (what move-analysis
                // reads for a var); a projected place uses `LoadPlace`/`Store`, with
                // the place flattened once so its indices are evaluated once.
                if let ExprKind::Identifier(name) = &place.kind {
                    let var = self.var(name);
                    let cur = self.fresh(span(place), Some(var));
                    self.emit(MirInstr::UseVar {
                        dest: cur,
                        var,
                        mode: UseMode::Copy,
                    });
                    let rhs = self.expr(value);
                    let res = self.fresh(span(place), None);
                    self.emit(MirInstr::BinOp {
                        op: *op,
                        dest: res,
                        a: cur,
                        b: rhs,
                    });
                    self.emit(MirInstr::DefVar {
                        var,
                        src: res,
                        source_annotation: None,
                    });
                } else {
                    let p = self.place(place);
                    let cur = self.fresh(span(place), None);
                    self.emit(MirInstr::LoadPlace {
                        dest: cur,
                        place: p.clone(),
                    });
                    let rhs = self.expr(value);
                    let res = self.fresh(span(place), None);
                    self.emit(MirInstr::BinOp {
                        op: *op,
                        dest: res,
                        a: cur,
                        b: rhs,
                    });
                    self.emit(MirInstr::Store { place: p, src: res });
                }
            }

            // --- Simple effectful statements -----------------------------------
            StmtKind::Raise(e) => {
                let src = self.expr(e);
                self.emit(MirInstr::Raise { src });
            }
            // `comptime N = e` is an ordinary `Int` binding at runtime.
            StmtKind::Comptime { name, value } => {
                let src = self.expr(value);
                let var = self.var(name);
                self.emit(MirInstr::DefVar {
                    var,
                    src,
                    source_annotation: None,
                });
            }
            // `pass` has no runtime effect. Imports were consumed by linking and
            // are no-ops in a lowered module body.
            StmtKind::Pass | StmtKind::Import { .. } | StmtKind::FromImport { .. } => {}

            // `try`/`except`/`else`/`finally` — each part lowers to a mini-CFG that
            // shares this function's slots; the VM runs them with `exec_try`
            // semantics. `cleanup` (the exceptional-edge drops) is filled by the
            // drop-elaboration pass.
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                // A `break`/`continue` that leaves the `try` (targeting an enclosing
                // loop) needs the outer loop's target block, which the self-contained
                // mini-CFG region can't name — refuse cleanly rather than build an
                // ill-formed region. (A `return` crossing out is fine: it surfaces as
                // a `Flow::Return` the block driver handles.)
                let crosses = region_crosses_control(body)
                    || except
                        .as_ref()
                        .is_some_and(|(_, b)| region_crosses_control(b))
                    || orelse.as_ref().is_some_and(|b| region_crosses_control(b))
                    || finalbody
                        .as_ref()
                        .is_some_and(|b| region_crosses_control(b));
                if crosses {
                    self.emit(MirInstr::Unsupported(
                        "try with break/continue crossing the try boundary".into(),
                    ));
                    return;
                }
                // Fallback path (a `try` whose enclosing loops are region-local, so
                // the HIR left it as an opaque `Stmt`): no escapable loops.
                self.emit_try(body, except, orelse, finalbody, &[], outer_map);
            }
            // A nested `def` that was lifted (registered in `nested`) is a no-op
            // here — its body is a separate function and calls are rewritten to it.
            StmtKind::Def { name, .. } if self.nested.contains_key(name) => {}
            // A nested `def` we couldn't lift (generic, calls a sibling, or nests
            // deeper), or a nested `struct`/`trait`, stays a clean `Unsupported`.
            StmtKind::Def { .. } | StmtKind::Struct { .. } | StmtKind::Trait { .. } => self.emit(
                MirInstr::Unsupported("nested def/struct/trait declaration".into()),
            ),

            // Tuple unpacking `a, b = t`: evaluate the tuple once, then bind each
            // target from its element (a NAME → `DefVar`; a place → `Store`).
            StmtKind::Unpack { targets, value } => {
                let tuple = self.expr(value);
                for (i, target) in targets.iter().enumerate() {
                    let idx = self.fresh(span(target), None);
                    self.emit(MirInstr::Const {
                        dest: idx,
                        k: Const::Int(i as i64),
                    });
                    let elem = self.fresh(span(target), None);
                    self.emit(MirInstr::Index {
                        dest: elem,
                        base: tuple,
                        index: idx,
                    });
                    match &target.kind {
                        ExprKind::Identifier(name) => {
                            let var = self.var(name);
                            self.emit(MirInstr::DefVar {
                                var,
                                src: elem,
                                source_annotation: None,
                            });
                        }
                        _ => {
                            let place = self.place(target);
                            self.emit(MirInstr::Store { place, src: elem });
                        }
                    }
                }
            }

            // --- Unreachable after the checker ---------------------------------
            // Parse-only statements are flagged `Unsupported`, so a checked program
            // never reaches MIR with them.
            StmtKind::With { .. } | StmtKind::ComptimeIf { .. } | StmtKind::ComptimeFor { .. } => {
                self.emit(MirInstr::Unsupported(format!(
                    "unchecked statement reached MIR lowering: {:?}",
                    s.kind
                )));
            }
            // These are lowered by `hir::Lower` directly (to instrs/terminators), so
            // they never arrive here wrapped in a `HirInstr::Stmt`.
            StmtKind::If { .. }
            | StmtKind::While { .. }
            | StmtKind::For { .. }
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Return(_)
            | StmtKind::VarDecl { .. }
            | StmtKind::Assign { .. }
            | StmtKind::Expr(_) => {
                self.emit(MirInstr::Unsupported(format!(
                    "malformed HIR statement instruction: {:?}",
                    s.kind
                )));
            }
        }
    }

    /// Lower a HIR block terminator; the branch/return operands are flattened into
    /// `self.cur` first, then the `MirTerm` references their result registers.
    fn lower_term(
        &mut self,
        t: &Terminator,
        map: &HashMap<hir::BlockId, MirBlockId>,
        outer_map: &HashMap<hir::BlockId, MirBlockId>,
    ) -> MirTerm {
        match t {
            Terminator::Jump(b) => MirTerm::Jump(map[b]),
            Terminator::Branch {
                cond,
                then_b,
                else_b,
            } => {
                let c = self.expr(cond); // the condition is evaluated at the end of this block
                MirTerm::Branch {
                    cond: c,
                    then_b: map[then_b],
                    else_b: map[else_b],
                }
            }
            Terminator::Return(e) => MirTerm::Return(e.as_ref().map(|e| {
                if self.returns_reference {
                    let place = self.place(e);
                    let dest = self.fresh(e.source_span(), Some(place.root));
                    self.emit(MirInstr::MakeRef { dest, place });
                    dest
                } else {
                    self.expr(e)
                }
            })),
            Terminator::FallOff => MirTerm::FallOff,
            // An outward `break`/`continue`: the target is an enclosing-function
            // block, resolved via `outer_map` (`cleanup` filled by drop elaboration).
            Terminator::EscapeJump(b) => MirTerm::EscapeJump {
                target: outer_map[b],
                cleanup: Vec::new(),
            },
        }
    }
}

/// Lower a whole HIR control-flow graph (one function body) into a `MirFunction`.
/// Each HIR block becomes a MIR block (same order); a single [`Flatten`] threads
/// the register counter, the variable interner (seeded from `cfg.vars` so IDs
/// agree with the HIR), and the span table across the whole function.
pub fn lower_cfg(cfg: &Cfg) -> MirFunction {
    lower_cfg_nested(
        cfg,
        &HashMap::new(),
        &crate::symbol::OverloadSets::default(),
        &HashMap::new(),
        false,
    )
}

/// [`lower_cfg`] with a nested-`def` registry in scope: a call to a registered
/// nested `def` is rewritten to its lifted function (captures prepended) and the
/// nested `def` statement lowers to nothing.
fn lower_cfg_nested(
    cfg: &Cfg,
    nested: &HashMap<String, NestedInfo>,
    overloads: &crate::symbol::OverloadSets,
    overload_targets: &HashMap<SourceSpan, String>,
    returns_reference: bool,
) -> MirFunction {
    let mut mir = MirFunction {
        blocks: Vec::new(),
        n_regs: 0,
        n_vars: cfg.vars.len(),
        var_names: cfg.vars.clone(),
        n_params: cfg.n_params,
        param_annotations: Vec::new(),
        owned_params: Vec::new(),
        ref_params: Vec::new(),
        returns_reference,
        spans: SpanTable::default(),
    };

    // One empty MIR block per HIR block; record the HIR→MIR index mapping so
    // terminators can translate their jump targets.
    let mut map: HashMap<hir::BlockId, MirBlockId> = HashMap::new();
    for hb in cfg.g.node_indices() {
        map.insert(hb, mir.blocks.len());
        mir.blocks.push(MirBlock {
            instrs: Vec::new(),
            term: MirTerm::Return(None),
        }); // placeholder term
    }

    {
        let mut fl = Flatten {
            f: &mut mir,
            cur: 0,
            next_reg: 0,
            vars: cfg.vars.clone(),
            nested: nested.clone(),
            overloads: overloads.clone(),
            overload_targets: overload_targets.clone(),
            aliases: HashMap::new(),
            runtime_aliases: std::collections::HashSet::new(),
            returns_reference,
        };
        for hb in cfg.g.node_indices() {
            fl.cur = map[&hb];
            for instr in &cfg.g[hb].instrs {
                // At the function level the "outer" map is this function's own map
                // (a `try`'s escape targets are this function's loop blocks).
                fl.lower_instr(instr, &map);
            }
            let fallback = Terminator::FallOff;
            let term = cfg.g[hb].term.as_ref().unwrap_or(&fallback);
            let mterm = fl.lower_term(term, &map, &map);
            fl.f.blocks[fl.cur].term = mterm;
        }
        fl.f.n_regs = fl.next_reg;
        // The MIR flattener may intern additional locals beyond the HIR's set
        // (short-circuit / iterator temporaries), so take the final interner.
        fl.f.n_vars = fl.vars.len();
        fl.f.var_names = fl.vars.clone();
    } // `fl` (the &mut borrow of `mir`) ends here

    mir
}

/// A whole program's worth of lowered functions, keyed by name. The synthetic
/// `__toplevel__` holds executable top-level statements; the VM runs it before
/// the program's zero-argument `main` entry point.
#[derive(Debug)]
pub struct MirProgram {
    pub functions: Vec<(String, MirFunction)>,
    /// Declaration facts needed by execution, normalized once while lowering.
    /// Backends consume this instead of rescanning the source AST.
    pub declarations: MirDeclarations,
    /// Violations of the checked-program contract discovered while lowering.
    /// Backends must reject a program with any entry rather than executing a
    /// guessed fallback representation.
    pub invariant_errors: Vec<String>,
}

fn checked_type_or_record(
    checked: &CheckedProgram,
    site: AnnotationSite,
    description: &str,
    invariant_errors: &mut Vec<String>,
) -> Ty {
    match checked.checked_type_at(&site) {
        Some(ty) => ty.clone(),
        None => {
            invariant_errors.push(format!("missing checked type for {description}"));
            Ty::None
        }
    }
}

#[derive(Debug, Default)]
pub struct MirDeclarations {
    pub structs: Vec<MirStructDeclaration>,
    pub functions: Vec<MirFunctionDeclaration>,
}

#[derive(Debug)]
pub struct MirStructDeclaration {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
    pub mut_self_methods: HashSet<String>,
    pub fieldwise_init: bool,
    pub param_decls: Vec<(String, bool)>,
}

#[derive(Debug)]
pub struct MirFunctionDeclaration {
    pub lowered_name: String,
    pub param_names: Vec<String>,
    pub param_types: Vec<Ty>,
    pub defaults: Vec<Option<CheckedConst>>,
    pub required: Vec<bool>,
    pub variadic: Option<Ty>,
    pub variadic_index: Option<usize>,
    pub kw_variadic: Option<Ty>,
    pub kw_variadic_index: Option<usize>,
    pub positional_only: Option<usize>,
    pub keyword_only: Option<usize>,
    pub param_decls: Vec<(String, bool)>,
}

mod nested;
use nested::*;

/// Lower a whole program (a top-level statement list) into per-function MIR.
///
/// Decision — **declarations are handled here, not inside a function body**: each
/// top-level `def` becomes its own `MirFunction`; each `struct` method becomes
/// `Struct.method`; a `trait`'s bodiless requirements produce nothing (default
/// methods are deferred). Remaining top-level statements form `__toplevel__`.
/// (Nested `def`s inside a body are still deferred — see `lower_stmt`.)
pub fn lower_program(program: &[Stmt]) -> Result<MirProgram, crate::error::TypeError> {
    let checked = crate::checker::check_program(program)?;
    Ok(lower_checked_program(&checked))
}

pub fn lower_checked_program(checked: &CheckedProgram) -> MirProgram {
    let program = checked.statements();
    let mut functions = Vec::new();
    let mut declarations = MirDeclarations::default();
    let mut invariant_errors = Vec::new();
    let mut toplevel: Vec<Stmt> = Vec::new();
    let overloads = crate::symbol::OverloadSets::scan(program);
    let overload_targets = checked.overload_targets().clone();

    for s in program {
        match &s.kind {
            StmtKind::Def {
                name,
                type_params,
                params,
                positional_only,
                keyword_only,
                ret,
                body,
                ..
            } => {
                let names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                let ptys = params.iter().map(|p| p.ty.clone()).collect();
                let owned = params.iter().map(|p| is_owned(&p.convention)).collect();
                let refp = params.iter().map(|p| is_ref(&p.convention)).collect();
                let lowered_name =
                    crate::symbol::lowered_def_name(name, type_params, params, &overloads);
                let variadic_idx = params.iter().position(|p| p.kind == ParamKind::Variadic);
                let kw_variadic_idx = params.iter().position(|p| p.kind == ParamKind::KwVariadic);
                let regular: Vec<_> = params
                    .iter()
                    .filter(|p| p.kind == ParamKind::Regular)
                    .collect();
                declarations.functions.push(MirFunctionDeclaration {
                    lowered_name: lowered_name.clone(),
                    param_names: regular.iter().map(|p| p.name.clone()).collect(),
                    param_types: regular
                        .iter()
                        .map(|p| {
                            checked_type_or_record(
                                checked,
                                AnnotationSite::FunctionParam {
                                    module: s.module.clone(),
                                    declaration: s.span,
                                    param: params
                                        .iter()
                                        .position(|candidate| std::ptr::eq(candidate, *p))
                                        .unwrap_or(params.len()),
                                },
                                &format!("parameter '{}' of function '{name}'", p.name),
                                &mut invariant_errors,
                            )
                        })
                        .collect(),
                    defaults: regular
                        .iter()
                        .map(|p| p.default.as_ref().and_then(CheckedConst::from_expr))
                        .collect(),
                    required: regular.iter().map(|p| p.default.is_none()).collect(),
                    variadic: variadic_idx.map(|i| {
                        checked_type_or_record(
                            checked,
                            AnnotationSite::FunctionParam {
                                module: s.module.clone(),
                                declaration: s.span,
                                param: i,
                            },
                            &format!("variadic parameter of function '{name}'"),
                            &mut invariant_errors,
                        )
                    }),
                    variadic_index: regular_marker_index(params, variadic_idx),
                    kw_variadic: kw_variadic_idx.map(|i| {
                        checked_type_or_record(
                            checked,
                            AnnotationSite::FunctionParam {
                                module: s.module.clone(),
                                declaration: s.span,
                                param: i,
                            },
                            &format!("keyword variadic parameter of function '{name}'"),
                            &mut invariant_errors,
                        )
                    }),
                    kw_variadic_index: kw_variadic_idx,
                    positional_only: regular_marker_index(params, *positional_only),
                    keyword_only: effective_keyword_only_index(params, *keyword_only, variadic_idx),
                    param_decls: crate::runtime::classify_param_decls(type_params),
                });
                lower_fn_nested(
                    FunctionLowering {
                        name: &lowered_name,
                        parameter_names: &names,
                        parameter_annotations: ptys,
                        owned_parameters: owned,
                        reference_parameters: refp,
                        returns_reference: matches!(ret, Some(SourceType::Ref { .. })),
                        body,
                        overloads: &overloads,
                        overload_targets: &overload_targets,
                    },
                    &mut functions,
                );
            }
            StmtKind::Struct {
                name,
                type_params,
                fields,
                methods,
                fieldwise_init,
                ..
            } => {
                let mut_self_methods = methods
                    .iter()
                    .filter(|m| {
                        matches!(
                            m.self_convention,
                            Some(ArgConvention::Mut | ArgConvention::Ref)
                        )
                    })
                    .map(|m| {
                        let method_name = crate::symbol::lifecycle_method_name(m);
                        let source = format!("{name}.{method_name}");
                        let lowered = crate::symbol::lowered_method_name(
                            &source,
                            type_params,
                            &m.params,
                            &overloads,
                        );
                        if lowered == source {
                            method_name.to_string()
                        } else {
                            lowered
                        }
                    })
                    .collect();
                declarations.structs.push(MirStructDeclaration {
                    name: name.clone(),
                    fields: fields
                        .iter()
                        .enumerate()
                        .map(|(field_index, field)| {
                            (
                                field.name.clone(),
                                checked_type_or_record(
                                    checked,
                                    AnnotationSite::StructField {
                                        module: s.module.clone(),
                                        declaration: s.span,
                                        field: field_index,
                                    },
                                    &format!("field '{}' of struct '{name}'", field.name),
                                    &mut invariant_errors,
                                ),
                            )
                        })
                        .collect(),
                    mut_self_methods,
                    fieldwise_init: *fieldwise_init,
                    param_decls: crate::runtime::classify_param_decls(type_params),
                });
                for (method_index, m) in methods.iter().enumerate() {
                    let method_name = crate::symbol::lifecycle_method_name(m);
                    let source_mangled = format!("{name}.{method_name}");
                    let mangled = crate::symbol::lowered_method_name(
                        &source_mangled,
                        type_params,
                        &m.params,
                        &overloads,
                    );
                    let variadic_idx = m
                        .params
                        .iter()
                        .position(|param| param.kind == ParamKind::Variadic);
                    let regular: Vec<_> = m
                        .params
                        .iter()
                        .filter(|param| param.kind == ParamKind::Regular)
                        .collect();
                    declarations.functions.push(MirFunctionDeclaration {
                        lowered_name: mangled.clone(),
                        param_names: regular.iter().map(|param| param.name.clone()).collect(),
                        param_types: regular
                            .iter()
                            .map(|param| {
                                checked_type_or_record(
                                    checked,
                                    AnnotationSite::MethodParam {
                                        module: s.module.clone(),
                                        declaration: s.span,
                                        method: method_index,
                                        param: m
                                            .params
                                            .iter()
                                            .position(|candidate| std::ptr::eq(candidate, *param))
                                            .unwrap_or(m.params.len()),
                                    },
                                    &format!(
                                        "parameter '{}' of method '{source_mangled}'",
                                        param.name
                                    ),
                                    &mut invariant_errors,
                                )
                            })
                            .collect(),
                        defaults: regular
                            .iter()
                            .map(|param| param.default.as_ref().and_then(CheckedConst::from_expr))
                            .collect(),
                        required: regular
                            .iter()
                            .map(|param| param.default.is_none())
                            .collect(),
                        variadic: variadic_idx.map(|index| {
                            checked_type_or_record(
                                checked,
                                AnnotationSite::MethodParam {
                                    module: s.module.clone(),
                                    declaration: s.span,
                                    method: method_index,
                                    param: index,
                                },
                                &format!("variadic parameter of method '{source_mangled}'"),
                                &mut invariant_errors,
                            )
                        }),
                        variadic_index: regular_marker_index(&m.params, variadic_idx),
                        kw_variadic: None,
                        kw_variadic_index: None,
                        positional_only: regular_marker_index(&m.params, m.positional_only),
                        keyword_only: effective_keyword_only_index(
                            &m.params,
                            m.keyword_only,
                            variadic_idx,
                        ),
                        param_decls: Vec::new(),
                    });
                    // A method's receiver `self` is the implicit first parameter,
                    // followed by the declared params.
                    let mut names: Vec<String> = Vec::new();
                    let mut ptys: Vec<SourceType> = Vec::new();
                    let mut owned: Vec<bool> = Vec::new();
                    let mut refp: Vec<bool> = Vec::new();
                    if m.has_self {
                        names.push("self".to_string());
                        // `self` is the struct type; `coerce` is identity on it, so
                        // the exact type only needs to be non-numeric.
                        ptys.push(SourceType::Named(name.clone(), Vec::new()));
                        owned.push(is_owned(&m.self_convention));
                        refp.push(is_ref(&m.self_convention));
                    }
                    names.extend(m.params.iter().map(|p| p.name.clone()));
                    ptys.extend(m.params.iter().map(|p| p.ty.clone()));
                    owned.extend(m.params.iter().map(|p| is_owned(&p.convention)));
                    refp.extend(m.params.iter().map(|p| is_ref(&p.convention)));
                    lower_fn_nested(
                        FunctionLowering {
                            name: &mangled,
                            parameter_names: &names,
                            parameter_annotations: ptys,
                            owned_parameters: owned,
                            reference_parameters: refp,
                            returns_reference: matches!(m.ret, Some(SourceType::Ref { .. })),
                            body: &m.body,
                            overloads: &overloads,
                            overload_targets: &overload_targets,
                        },
                        &mut functions,
                    );
                }
            }
            // A `trait`'s requirements have no body (`...`); nothing to lower yet.
            StmtKind::Trait { .. } => {}
            _ => toplevel.push(s.clone()),
        }
    }

    functions.push((
        "__toplevel__".to_string(),
        lower_cfg_nested(
            &Cfg::build(&toplevel),
            &HashMap::new(),
            &overloads,
            &overload_targets,
            false,
        ),
    ));
    MirProgram {
        functions,
        declarations,
        invariant_errors,
    }
}
