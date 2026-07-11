//! Phase 2 of the compiler frontend: flatten the HIR's nested expressions into
//! **A-Normal Form** (three-address code) — every subexpression becomes a
//! `MirInstr` writing a fresh [`Reg`], so `foo(bar(x))` becomes
//! `t0 = bar(x); t1 = foo(t0)`. The flattened form is what Phase 4's dataflow
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
    ArgConvention, Dtype, Expr, ExprKind, FnParam, InfixOp, ParamArg, PrefixOp, Stmt, StmtKind,
    TStringPart, Type,
};
use crate::checker::resolve_overload_targets;
use crate::token::DUMMY_SPAN;
use std::collections::HashSet;

/// Whether an argument convention transfers ownership to the callee (`owned`, or
/// the destructor's `deinit`).
fn is_owned(c: &Option<ArgConvention>) -> bool {
    matches!(c, Some(ArgConvention::Owned | ArgConvention::Deinit))
}

/// Whether a `try` region's statements contain a `break`/`continue` that **leaves**
/// the region — targeting a loop *outside* it. Such an escape would need to name the
/// outer loop's target block, which the self-contained mini-CFG region can't express
/// (unlike a `return`, which surfaces as a `Flow::Return` the block driver handles).
/// Nested loops absorb their own `break`/`continue` (tracked via `loop_depth`);
/// nested `def`/`struct` bodies have their own control flow and are not scanned.
fn region_crosses_control(body: &[Stmt]) -> bool {
    fn walk(stmts: &[Stmt], loop_depth: usize) -> bool {
        stmts.iter().any(|s| match &s.kind {
            StmtKind::Break | StmtKind::Continue => loop_depth == 0,
            StmtKind::If { branches, orelse } => {
                branches.iter().any(|(_, b)| walk(b, loop_depth))
                    || orelse.as_ref().is_some_and(|b| walk(b, loop_depth))
            }
            StmtKind::While { body, .. } | StmtKind::For { body, .. } => walk(body, loop_depth + 1),
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                walk(body, loop_depth)
                    || except.as_ref().is_some_and(|(_, b)| walk(b, loop_depth))
                    || orelse.as_ref().is_some_and(|b| walk(b, loop_depth))
                    || finalbody.as_ref().is_some_and(|b| walk(b, loop_depth))
            }
            _ => false,
        })
    }
    walk(body, 0)
}

/// Whether an argument convention is a written-back reference (`mut`/`ref`).
fn is_ref(c: &Option<ArgConvention>) -> bool {
    matches!(c, Some(ArgConvention::Mut | ArgConvention::Ref))
}
use crate::hir::{self, Cfg, HirInstr, Terminator, VarId};
use std::collections::HashMap;

/// A virtual ("infinite") register — a fresh one per intermediate value (SSA-ish).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reg(pub u32);

/// Index of a basic block within a [`MirFunction`]'s `blocks`.
pub type MirBlockId = usize;

/// A source byte range `(start, end)` — re-exported from [`crate::token`], the
/// canonical span type stamped by the parser onto every AST node.
pub use crate::token::Span;

/// How a variable is used at a given site (set from `^` and param conventions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseMode {
    Copy,
    Move,
    BorrowShared,
    BorrowMut,
}

/// A compile-time-known literal.
#[derive(Debug, Clone)]
pub enum Const {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    None,
}

/// The callee of a `MirInstr::Call` — a function/struct-constructor/builtin name.
/// (Resolution to a concrete target happens in the backend's assembler.)
#[derive(Debug, Clone)]
pub struct FuncRef(pub String);
impl FuncRef {
    pub fn named(name: &str) -> FuncRef {
        FuncRef(name.to_string())
    }
}

/// One step of a **place** projection: a field of a struct, or a subscript. A
/// place is a *writable location* — a root variable followed by projections — as
/// opposed to an rvalue (a computed register). This mirrors `rustc` MIR's
/// `Place`/`Projection` split, and is what a write / read-modify-write targets.
#[derive(Debug, Clone)]
pub enum Proj {
    Field(String),
    Index(Reg), // the subscript index, flattened to a register (evaluated once)
}

/// A writable location: a root variable plus a chain of projections
/// (`p.items[i].x` = root `p`, proj `[Field("items"), Index(i), Field("x")]`).
#[derive(Debug, Clone)]
pub struct MirPlace {
    pub root: VarId,
    pub proj: Vec<Proj>,
}

/// A single three-address instruction. Each value-producing instruction writes a
/// fresh `dest` register; control flow lives in the block's [`MirTerm`].
#[derive(Debug, Clone)]
pub enum MirInstr {
    Const {
        dest: Reg,
        k: Const,
    },
    /// `x`, `x^`, `borrow x`, … — a use of a variable, tagged with how (`mode`).
    UseVar {
        dest: Reg,
        var: VarId,
        mode: UseMode,
    },
    /// A **partial move** `p.a^` — transfer one sub-place (a pure field chain,
    /// no dynamic index) out of a variable, reading its value into `dest`. The
    /// ownership analysis tracks this at place granularity (moving `p.a` leaves
    /// `p.b` usable); at runtime the field slot is left a tombstone so a later
    /// drop of the whole struct skips it (no double-drop). Whole-variable moves
    /// stay `UseVar { mode: Move }`; an indexed transfer falls back to a plain
    /// read (the move is not modeled — conservative for dynamic indices).
    MovePlace {
        dest: Reg,
        place: MirPlace,
    },
    /// `var := <register>` — (re)define a variable slot from a register (lowered
    /// from a HIR `Bind`). The write paired with `UseVar`; Phase 4 reads it as a
    /// dataflow *def* (transitions the var to `Owned`). `ty` is a declaration's
    /// annotation (`var x: T = …`) so a backend can materialize a numeric literal
    /// to `T` (the tree-walker's `coerce`); `None` = inferred `var`/reassignment,
    /// which keeps the binding's existing type (`coerce_like`).
    DefVar {
        var: VarId,
        src: Reg,
        ty: Option<Type>,
    },
    UnOp {
        op: PrefixOp,
        dest: Reg,
        a: Reg,
    },
    BinOp {
        op: InfixOp,
        dest: Reg,
        a: Reg,
        b: Reg,
    },
    /// A free-function / constructor / builtin call. `args` are the flattened
    /// positional arguments; `kwargs` the keyword arguments (`name = value`). The
    /// backend matches them to the callee's parameter slots (filling defaults,
    /// collecting `*args`) via `checker::match_call_slots`.
    /// A free-function call. `arg_places[i]` is `Some` when positional argument `i`
    /// is a simple place (a variable or field chain, no dynamic index), so a
    /// `mut`/`ref` parameter can write its final value back to the caller; `None`
    /// otherwise (a temporary, or an indexed place).
    Call {
        dest: Reg,
        func: FuncRef,
        args: Vec<Reg>,
        kwargs: Vec<(String, Reg)>,
        arg_places: Vec<Option<MirPlace>>,
        /// The supplied compile-time parameter arguments (`Name[param_args](args)`),
        /// one entry per `[...]` slot: `Some(reg)` for a **value** parameter (a
        /// comptime `Int` expression, flattened to a register) and `None` for a
        /// **type** parameter (erased). The backend reifies the value arguments onto
        /// a constructed struct's `value_params` (type parameters stay erased). Empty
        /// for a plain call.
        param_arg_regs: Vec<Option<Reg>>,
    },
    /// A method call `recv.method(args)`. `recv_place` is `Some` when the receiver
    /// is a writable place (a variable / field-index chain), so a `mut self` method
    /// or an in-place `List` mutator can write the updated receiver back; `None`
    /// for a temporary receiver (a call result), on which only read-only methods
    /// are valid (the checker guarantees this).
    MethodCall {
        dest: Reg,
        recv: Reg,
        method: String,
        resolved: Option<String>,
        args: Vec<Reg>,
        recv_place: Option<MirPlace>,
        /// Like `Call::arg_places`: `arg_places[i]` is `Some` when ordinary
        /// argument `i` is a simple place, so a method's `mut`/`ref` ordinary
        /// parameter can write its final value back to the caller.
        arg_places: Vec<Option<MirPlace>>,
    },
    /// Struct/field *read* `base.field` inside an rvalue (name-based; the backend
    /// resolves layout). Field/index *writes* go through `Store`/a `MirPlace`.
    GetField {
        dest: Reg,
        base: Reg,
        field: String,
    },
    /// Subscript *read* `base[index]` (List/Tuple/SIMD lane) inside an rvalue.
    Index {
        dest: Reg,
        base: Reg,
        index: Reg,
    },
    /// Slice `object[lower:upper:step]` (List/String) → a new value. Each bound is
    /// optional (absent = a direction-aware default).
    Slice {
        dest: Reg,
        object: Reg,
        lower: Option<Reg>,
        upper: Option<Reg>,
        step: Option<Reg>,
    },
    /// `place = src` — a write through a place (`p.x = e`, `xs[i] = e`, nested).
    Store {
        place: MirPlace,
        src: Reg,
    },
    /// Read a place into a register — for a read-modify-write (`place OP= e`),
    /// where the place (and its indices) must be evaluated exactly once.
    LoadPlace {
        dest: Reg,
        place: MirPlace,
    },
    /// Aggregate construction from already-flattened element registers.
    MakeList {
        dest: Reg,
        elems: Vec<Reg>,
    },
    MakeTuple {
        dest: Reg,
        elems: Vec<Reg>,
    },
    /// SIMD construction `SIMD[DType.<dt>, width](elems)` (or a scalar-alias like
    /// `Int32(x)`). The element `dtype`/`width` — compile-time parameters the MIR
    /// is otherwise untyped about — are resolved here at lowering; `elems` are the
    /// lane values (exactly `width`, or one to splat).
    MakeSimd {
        dest: Reg,
        dtype: Dtype,
        width: usize,
        elems: Vec<Reg>,
    },
    /// `raise <src>` — raise an error value. Propagates as an exceptional outcome
    /// (the VM unwinds to the nearest enclosing [`MirInstr::Try`] handler).
    Raise {
        src: Reg,
    },
    /// A `try`/`except`/`else`/`finally` region, lowered structurally (mirroring the
    /// tree-walker's `exec_try`). Each sub-part is a self-contained mini-CFG (a
    /// `Vec<MirBlock>` with local block ids, entry = block 0) that **shares this
    /// function's register and variable space** — so it addresses the same slots.
    /// `handler` is `Some((error_var, body))` when there is an `except` clause (the
    /// optional slot binds the caught error). `cleanup` lists the body-local
    /// variables to drop when the body unwinds (the exceptional-edge cleanup).
    Try {
        body: Vec<MirBlock>,
        handler: Option<(Option<VarId>, Vec<MirBlock>)>,
        orelse: Option<Vec<MirBlock>>,
        finalbody: Option<Vec<MirBlock>>,
        cleanup: Vec<VarId>,
    },
    /// An ASAP destructor on a register (reserved for the future Op/assembler VM).
    Drop {
        reg: Reg,
    },
    /// Drop the value in a variable slot — spliced in by the Phase 4 liveness pass
    /// at a variable's last use (ASAP destruction). Runs the value's `__del__` (and
    /// its fields', in reverse order) and leaves the slot empty. A no-op for values
    /// without a destructor, so it never changes observable behaviour except when a
    /// struct defines `__del__`.
    DropVar {
        var: VarId,
    },
    /// A construct the MIR/backends don't lower yet (a `try` with its exceptional
    /// edges, a nested declaration). Kept as an explicit node — rather than a
    /// lowering-time `panic!` — so a backend can report a clean error instead of
    /// crashing on an otherwise-valid program.
    Unsupported(String),
    /// Iterator protocol: normalize `iter` to an iterator (a user struct's
    /// `__iter__()`); a no-op for a built-in `range`/`List`.
    GetIter {
        iter: VarId,
    },
    /// Iterator protocol (`for` loops): read whether the iterator variable `iter`
    /// yields another element into `dest` (a `Bool`) — a pure read.
    HasNext {
        dest: Reg,
        iter: VarId,
    },
    /// Iterator protocol: bind the current element into `dest` and advance the
    /// iterator variable `iter` in place (a mutating read).
    Next {
        dest: Reg,
        iter: VarId,
    },
}

/// How a basic block hands off control. Block targets are indices into
/// `MirFunction::blocks`; values are registers.
#[derive(Debug, Clone)]
pub enum MirTerm {
    Jump(MirBlockId),
    Branch {
        cond: Reg,
        then_b: MirBlockId,
        else_b: MirBlockId,
    },
    Return(Option<Reg>),
    /// Normal fall-through end of a `try` sub-region (see [`hir::Terminator::FallOff`]).
    /// The VM's region runner reads it as "completed normally". Never appears in a
    /// function body's blocks.
    FallOff,
    /// A `break`/`continue` inside a `try` region that targets an enclosing
    /// **function** loop: `target` is that loop's exit/header block in the
    /// *enclosing function*'s `blocks` (not the region's). The VM propagates it out
    /// as a `Flow::Jump(target)` — running each `finally` on the way — until the
    /// function driver jumps there. `cleanup` lists the region-body-local variables
    /// to drop when this escape edge is taken (filled by drop elaboration).
    EscapeJump {
        target: MirBlockId,
        cleanup: Vec<VarId>,
    },
}

#[derive(Debug, Clone)]
pub struct MirBlock {
    pub instrs: Vec<MirInstr>,
    pub term: MirTerm,
}

#[derive(Debug)]
pub struct MirFunction {
    pub blocks: Vec<MirBlock>,
    pub n_regs: u32,
    /// Number of variable slots (the interner size). A frame allocates this many
    /// var cells; `UseVar`/`DefVar` index into them.
    pub n_vars: usize,
    /// The name of each variable slot (`var_names[id]` is `VarId` `id`'s source
    /// name; synthetic `$…` names for compiler temporaries). For diagnostics — the
    /// ownership analysis names the offending variable.
    pub var_names: Vec<String>,
    /// Number of leading vars that are parameters (`vars[0..n_params]`), bound from
    /// the call's arguments in declaration order — the VM call ABI.
    pub n_params: usize,
    /// The declared type of each parameter (same order/length as the params), so a
    /// caller can `coerce` a numeric-literal argument to it — matching the
    /// tree-walker, which coerces at param binding. Empty for `__toplevel__`.
    pub param_types: Vec<Type>,
    /// Whether each parameter is `owned` (the callee takes ownership, so it drops
    /// the value — unlike a borrowed `read`/`mut` parameter). Same order as the
    /// params; the caller transfers with `^`, so its own drop is skipped.
    pub owned_params: Vec<bool>,
    /// Whether each parameter is a `mut`/`ref` **reference** (its final value is
    /// written back to the caller). `self` (handled via a method's `recv_place`) is
    /// always `false` here. Same order as the params.
    pub ref_params: Vec<bool>,
    pub spans: SpanTable,
}

/// Maps each generated register to its source span and (if it names one) the
/// origin variable — so borrow-checker diagnostics can point at real code.
#[derive(Debug, Default)]
pub struct SpanTable(pub HashMap<u32 /*reg*/, (Span, Option<VarId>)>);

/// An expression's source span, stamped by the parser (`ast::Expr.span`). Fed
/// into the [`SpanTable`] so each temporary can be traced back to its origin.
fn span(e: &Expr) -> Span {
    e.span
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
    overload_targets: HashMap<Span, String>,
}

impl Flatten<'_> {
    fn fresh(&mut self, span: Span, origin: Option<VarId>) -> Reg {
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
    /// both for observable side effects and for Phase 4 ownership, where a moved
    /// operand on the not-taken side must not count as moved.)
    fn short_circuit(&mut self, op: InfixOp, a: &Expr, b: &Expr, span: Span) -> Reg {
        let ra = self.expr(a);
        let result = self.fresh_var();
        // Seed the result with the left operand's value: for `and` a false `ra`
        // is the answer; for `or` a true `ra` is. The rhs block overwrites it.
        self.emit(MirInstr::DefVar {
            var: result,
            src: ra,
            ty: None,
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
            ty: None,
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
    fn ternary(&mut self, cond: &Expr, then_e: &Expr, else_e: &Expr, sp: Span) -> Reg {
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
            ty: None,
        });
        self.f.blocks[self.cur].term = MirTerm::Jump(merge_blk);
        self.cur = else_blk;
        let re = self.expr(else_e);
        self.emit(MirInstr::DefVar {
            var: result,
            src: re,
            ty: None,
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
    fn compare_chain(&mut self, first: &Expr, rest: &[(InfixOp, Expr)], sp: Span) -> Reg {
        let result = self.fresh_var();
        let merge_blk = self.new_block();
        let mut prev = self.expr(first);
        for (i, (op, operand)) in rest.iter().enumerate() {
            let cur = self.expr(operand);
            let cmp = self.fresh(sp, None);
            self.emit(MirInstr::BinOp {
                op: *op,
                dest: cmp,
                a: prev,
                b: cur,
            });
            self.emit(MirInstr::DefVar {
                var: result,
                src: cmp,
                ty: None,
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
            // `Borrow*`/`Move` per the callee's convention (Phase 4).
            ExprKind::Identifier(name) => {
                let var = self.var(name);
                let d = self.fresh(span(e), Some(var));
                self.emit(MirInstr::UseVar {
                    dest: d,
                    var,
                    mode: UseMode::Copy,
                });
                d
            }
            // `x^`: a move out of a variable. `p.a^` (a pure field chain) is a
            // **partial move** of that field; a move through an indexed place is
            // identity for now (conservative — Phase 4 does not model it).
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
            // NOTE: keyword args + default-slot matching (checker::match_call_slots)
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
                // evaluated before the call arguments (matching the tree-walker): a
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

            // The walrus `:=` type-checks (the checker passes it through for the
            // evaluator to flag), so it *can* reach MIR — lower it to a clean
            // `Unsupported` (never a panic), so the compiler path and the ownership
            // analysis handle it gracefully while the evaluator reports it.
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
            ExprKind::TypeApply { .. } | ExprKind::TString { .. } => {
                unreachable!("parse-only expression rejected by the checker before MIR: {e:?}")
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
            HirInstr::Bind { dest, expr, ty } => {
                let src = self.expr(expr);
                self.emit(MirInstr::DefVar {
                    var: *dest,
                    src,
                    ty: ty.clone(),
                });
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
                    unreachable!("HirInstr::Try always wraps a `try` statement");
                }
            }
            HirInstr::Drop(_) => {
                unreachable!("HirInstr::Drop is inserted by Phase 4, not present during lowering")
            }
            // Iterator protocol: compute into a register, then store to the target
            // variable (so the header's branch can read `has_next` as a `UseVar`,
            // and the body binds the loop variable).
            HirInstr::GetIter { iter } => {
                self.emit(MirInstr::GetIter { iter: *iter });
            }
            HirInstr::HasNext { iter, dest } => {
                let r = self.fresh(DUMMY_SPAN, None);
                self.emit(MirInstr::HasNext {
                    dest: r,
                    iter: *iter,
                });
                self.emit(MirInstr::DefVar {
                    var: *dest,
                    src: r,
                    ty: None,
                });
            }
            HirInstr::Next { iter, dest } => {
                let r = self.fresh(DUMMY_SPAN, Some(*iter));
                self.emit(MirInstr::Next {
                    dest: r,
                    iter: *iter,
                });
                self.emit(MirInstr::DefVar {
                    var: *dest,
                    src: r,
                    ty: None,
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
            ExprKind::Identifier(name) => MirPlace {
                root: self.var(name),
                proj: Vec::new(),
            },
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
            other => unreachable!("assignment place must be rooted at a variable, got {other:?}"),
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
            param_types: Vec::new(),
            owned_params: Vec::new(),
            ref_params: Vec::new(),
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
            };
            for hb in region_cfg.g.node_indices() {
                fl.cur = map[&hb];
                for instr in &region_cfg.g[hb].instrs {
                    fl.lower_instr(instr, outer_map);
                }
                let term = region_cfg.g[hb]
                    .term
                    .as_ref()
                    .expect("Phase 1 seals every block");
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
            ExprKind::Identifier(name) => Some(MirPlace {
                root: self.var(name),
                proj: Vec::new(),
            }),
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
                Some(MirPlace {
                    root: self.var(root),
                    proj: Vec::new(),
                })
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
            ExprKind::Identifier(name) => Some(MirPlace {
                root: self.var(name),
                proj: Vec::new(),
            }),
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
                        ty: None,
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
                self.emit(MirInstr::DefVar { var, src, ty: None });
            }
            // No runtime effect: `pass`, and imports (mojito has no module system,
            // so imports are no-ops — matching the checker/evaluator).
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
                                ty: None,
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
                unreachable!("parse-only statement rejected by the checker before MIR: {s:?}")
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
                unreachable!("handled by hir::Lower directly, not via HirInstr::Stmt: {s:?}")
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
            Terminator::Return(e) => MirTerm::Return(e.as_ref().map(|e| self.expr(e))),
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
    )
}

/// [`lower_cfg`] with a nested-`def` registry in scope: a call to a registered
/// nested `def` is rewritten to its lifted function (captures prepended) and the
/// nested `def` statement lowers to nothing.
fn lower_cfg_nested(
    cfg: &Cfg,
    nested: &HashMap<String, NestedInfo>,
    overloads: &crate::symbol::OverloadSets,
    overload_targets: &HashMap<Span, String>,
) -> MirFunction {
    let mut mir = MirFunction {
        blocks: Vec::new(),
        n_regs: 0,
        n_vars: cfg.vars.len(),
        var_names: cfg.vars.clone(),
        n_params: cfg.n_params,
        param_types: Vec::new(), // filled by `lower_program` where signatures are known
        owned_params: Vec::new(),
        ref_params: Vec::new(),
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
        };
        for hb in cfg.g.node_indices() {
            fl.cur = map[&hb];
            for instr in &cfg.g[hb].instrs {
                // At the function level the "outer" map is this function's own map
                // (a `try`'s escape targets are this function's loop blocks).
                fl.lower_instr(instr, &map);
            }
            let term = cfg.g[hb].term.as_ref().expect("Phase 1 seals every block");
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
/// `__toplevel__` holds the module's top-level statements (mirroring the
/// evaluator's top-level + `main()` model).
#[derive(Debug)]
pub struct MirProgram {
    pub functions: Vec<(String, MirFunction)>,
}

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
struct FunctionLowering<'a> {
    name: &'a str,
    parameter_names: &'a [String],
    parameter_types: Vec<Type>,
    owned_parameters: Vec<bool>,
    reference_parameters: Vec<bool>,
    body: &'a [Stmt],
    overloads: &'a crate::symbol::OverloadSets,
    overload_targets: &'a HashMap<Span, String>,
}

fn lower_fn_nested(request: FunctionLowering<'_>, out: &mut Vec<(String, MirFunction)>) {
    let FunctionLowering {
        name,
        parameter_names: param_names,
        parameter_types: param_types,
        owned_parameters: owned_params,
        reference_parameters: ref_params,
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
    let mut f = lower_cfg_nested(&cfg, &registry, overloads, overload_targets);
    f.param_types = param_types;
    f.owned_params = owned_params;
    f.ref_params = ref_params;
    out.push((name.to_string(), f));

    let cap_ty = Type::Named("$capture".to_string(), Vec::new());
    for (ds, captures, mangled) in liftable {
        if let StmtKind::Def {
            params: dparams,
            body: dbody,
            ..
        } = &ds.kind
        {
            let mut names: Vec<String> = captures.clone();
            names.extend(dparams.iter().map(|p| p.name.clone()));
            let mut ptys: Vec<Type> = vec![cap_ty.clone(); captures.len()];
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
            let mut nf = lower_cfg_nested(&ncfg, &registry, overloads, overload_targets);
            nf.param_types = ptys;
            nf.owned_params = owned2;
            nf.ref_params = refp2;
            out.push((mangled, nf));
        }
    }
}

/// Lower a whole program (a top-level statement list) into per-function MIR.
///
/// Decision — **declarations are handled here, not inside a function body**: each
/// top-level `def` becomes its own `MirFunction`; each `struct` method becomes
/// `Struct.method`; a `trait`'s bodiless requirements produce nothing (default
/// methods are deferred). Remaining top-level statements form `__toplevel__`.
/// (Nested `def`s inside a body are still deferred — see `lower_stmt`.)
pub fn lower_program(program: &[Stmt]) -> MirProgram {
    let mut functions = Vec::new();
    let mut toplevel: Vec<Stmt> = Vec::new();
    let overloads = crate::symbol::OverloadSets::scan(program);
    let overload_targets = resolve_overload_targets(program).unwrap_or_default();

    for s in program {
        match &s.kind {
            StmtKind::Def {
                name, params, body, ..
            } => {
                let names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                let ptys = params.iter().map(|p| p.ty.clone()).collect();
                let owned = params.iter().map(|p| is_owned(&p.convention)).collect();
                let refp = params.iter().map(|p| is_ref(&p.convention)).collect();
                let lowered_name = crate::symbol::lowered_def_name(name, params, &overloads);
                lower_fn_nested(
                    FunctionLowering {
                        name: &lowered_name,
                        parameter_names: &names,
                        parameter_types: ptys,
                        owned_parameters: owned,
                        reference_parameters: refp,
                        body,
                        overloads: &overloads,
                        overload_targets: &overload_targets,
                    },
                    &mut functions,
                );
            }
            StmtKind::Struct { name, methods, .. } => {
                for m in methods {
                    let method_name = crate::symbol::lifecycle_method_name(m);
                    let source_mangled = format!("{name}.{method_name}");
                    let mangled =
                        crate::symbol::lowered_method_name(&source_mangled, &m.params, &overloads);
                    // A method's receiver `self` is the implicit first parameter,
                    // followed by the declared params.
                    let mut names: Vec<String> = Vec::new();
                    let mut ptys: Vec<Type> = Vec::new();
                    let mut owned: Vec<bool> = Vec::new();
                    let mut refp: Vec<bool> = Vec::new();
                    if m.has_self {
                        names.push("self".to_string());
                        // `self` is the struct type; `coerce` is identity on it, so
                        // the exact type only needs to be non-numeric.
                        ptys.push(Type::Named(name.clone(), Vec::new()));
                        owned.push(is_owned(&m.self_convention));
                        refp.push(false); // `self` is handled via `recv_place`, not `ref_params`
                    }
                    names.extend(m.params.iter().map(|p| p.name.clone()));
                    ptys.extend(m.params.iter().map(|p| p.ty.clone()));
                    owned.extend(m.params.iter().map(|p| is_owned(&p.convention)));
                    refp.extend(m.params.iter().map(|p| is_ref(&p.convention)));
                    lower_fn_nested(
                        FunctionLowering {
                            name: &mangled,
                            parameter_names: &names,
                            parameter_types: ptys,
                            owned_parameters: owned,
                            reference_parameters: refp,
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
        ),
    ));
    MirProgram { functions }
}
