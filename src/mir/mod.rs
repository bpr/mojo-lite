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
pub mod verify;

/// An expression's source span, stamped by the parser (`ast::Expr.span`). Fed
/// into the [`SpanTable`] so each temporary can be traced back to its origin.
fn span(e: &Expr) -> SourceSpan {
    e.source_span()
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
    captures: Vec<NestedCapture>,
}

#[derive(Clone, PartialEq, Eq)]
struct NestedCapture {
    name: String,
    kind: crate::ast::CaptureKind,
}

#[derive(Clone)]
struct ExprFacts {
    ty: Option<Ty>,
    place_ty: Option<Ty>,
    owner: Option<crate::origin::OwnerId>,
    raises: Option<Ty>,
    adjustments: Vec<crate::SemanticAdjustment>,
    comprehension_bindings: Vec<crate::checked::CheckedComprehensionBinding>,
}

fn expression_children(expression: &Expr) -> Vec<&Expr> {
    match &expression.kind {
        ExprKind::Prefix(_, value)
        | ExprKind::Transfer(value)
        | ExprKind::Spread(value)
        | ExprKind::Named { value, .. } => {
            vec![value]
        }
        ExprKind::Infix(_, left, right)
        | ExprKind::Index {
            object: left,
            index: right,
        } => {
            vec![left, right]
        }
        ExprKind::Call { args, kwargs, .. } => args
            .iter()
            .chain(kwargs.iter().map(|argument| &argument.value))
            .collect(),
        ExprKind::Invoke {
            callee,
            args,
            kwargs,
            ..
        } => std::iter::once(callee.as_ref())
            .chain(args.iter())
            .chain(kwargs.iter().map(|argument| &argument.value))
            .collect(),
        ExprKind::Member { object, .. } => vec![object],
        ExprKind::MethodCall {
            object,
            args,
            kwargs,
            ..
        } => std::iter::once(object.as_ref())
            .chain(args.iter())
            .chain(kwargs.iter().map(|argument| &argument.value))
            .collect(),
        ExprKind::ListLit(values) | ExprKind::TupleLit(values) => values.iter().collect(),
        ExprKind::BraceLit(values) => values
            .iter()
            .flat_map(|(key, value)| std::iter::once(key).chain(value.iter()))
            .collect(),
        ExprKind::Comprehension {
            key,
            value,
            clauses,
            ..
        } => clauses
            .iter()
            .map(|clause| match clause {
                crate::ast::ComprehensionClause::For { iter, .. } => iter.as_ref(),
                crate::ast::ComprehensionClause::If(condition) => condition.as_ref(),
            })
            .chain(key.iter().map(Box::as_ref))
            .chain(std::iter::once(value.as_ref()))
            .collect(),
        ExprKind::IfExpr {
            cond,
            then_branch,
            else_branch,
        } => vec![cond, then_branch, else_branch],
        ExprKind::Compare { first, rest } => std::iter::once(first.as_ref())
            .chain(rest.iter().map(|(_, value)| value))
            .collect(),
        ExprKind::Slice {
            object,
            lower,
            upper,
            step,
            ..
        } => std::iter::once(object.as_ref())
            .chain(
                [lower, upper, step]
                    .into_iter()
                    .filter_map(|value| value.as_deref()),
            )
            .collect(),
        ExprKind::MultiIndex { object, args } => {
            let mut children = vec![object.as_ref()];
            for argument in args {
                match argument {
                    crate::ast::SubscriptArg::Index(value) => children.push(value),
                    crate::ast::SubscriptArg::Slice {
                        lower, upper, step, ..
                    } => {
                        children.extend([lower, upper, step].into_iter().flatten().map(Box::as_ref))
                    }
                }
            }
            children
        }
        ExprKind::TString { parts, .. } => parts
            .iter()
            .filter_map(|part| match part {
                TStringPart::Expr(value) => Some(value.as_ref()),
                TStringPart::Literal(_) => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn statement_expression_roots(statement: &Stmt) -> Vec<&Expr> {
    match &statement.kind {
        StmtKind::VarDecl { value, .. }
        | StmtKind::RefDecl { value, .. }
        | StmtKind::Assign { value, .. }
        | StmtKind::Comptime { value, .. }
        | StmtKind::Raise(value)
        | StmtKind::Expr(value) => vec![value],
        StmtKind::SetPlace { place, value } | StmtKind::AugAssign { place, value, .. } => {
            vec![place, value]
        }
        StmtKind::Unpack { targets, value } => {
            let mut roots: Vec<&Expr> = targets.iter().collect();
            roots.push(value);
            roots
        }
        StmtKind::Return(Some(value)) => vec![value],
        StmtKind::With { items, .. } => items.iter().map(|item| &item.context).collect(),
        _ => Vec::new(),
    }
}

fn index_hir_expression(
    syntax: &Expr,
    expression: &crate::hir::HirExpr,
    index: &mut HashMap<usize, ExprFacts>,
) {
    index.insert(
        syntax as *const Expr as usize,
        ExprFacts {
            ty: expression.ty.clone(),
            place_ty: expression.place.as_ref().map(|place| place.ty.clone()),
            owner: expression.place.as_ref().map(|place| place.owner),
            raises: expression.effects.raises.clone(),
            adjustments: expression.adjustments.clone(),
            comprehension_bindings: expression.comprehension_bindings.clone(),
        },
    );
    for (child_syntax, child) in expression_children(syntax)
        .into_iter()
        .zip(&expression.children)
    {
        index_hir_expression(child_syntax, child, index);
    }
}

/// Flattens nested `Expr`s into a block's instruction list. `cur` is the block
/// currently being appended to.
struct Flatten<'a> {
    f: &'a mut MirFunction,
    cur: MirBlockId,
    next_reg: u32,
    /// Interner: a variable name's first appearance assigns its `VarId`.
    vars: Vec<String>,
    /// Checked storage type for each interned variable. This is populated from
    /// checked parameters/uses and `HirInstr::Bind` before places are emitted.
    var_types: HashMap<VarId, Ty>,
    /// Runtime slots assigned to checked binding identities that do not have a
    /// statement-level HIR declaration, notably comprehension generators.
    owner_vars: HashMap<crate::origin::OwnerId, VarId>,
    /// Nested `def`s in scope (name → lifted target + captures); a call to one is
    /// rewritten to the mangled function with its captures prepended, and the
    /// nested `def` statement itself lowers to nothing.
    nested: HashMap<String, NestedInfo>,
    /// The program's overloaded declarations. Kept only for unchecked HIR tests;
    /// production lowering consumes `ResolveCallable` checked adjustments.
    overloads: crate::symbol::OverloadSets,
    checked_expressions: HashMap<crate::CheckedNodeId, crate::CheckedExpr>,
    /// Semantic facts indexed by the in-memory identity of the active HIR syntax
    /// tree. Maps are installed only while lowering that expression/statement;
    /// source spans are never used as semantic keys.
    active_semantics: Vec<HashMap<usize, ExprFacts>>,
    /// Local reference slot to its frozen owner place and permission.
    aliases: HashMap<VarId, (MirPlace, bool)>,
    runtime_aliases: std::collections::HashSet<VarId>,
    /// Persistent owner loans carried by reference-bearing aggregate variables.
    /// The runtime value contains the handles; this map transfers their static
    /// loans when an aggregate is moved or forwarded into a new binding.
    aggregate_loans: HashMap<VarId, Vec<(MirPlace, bool)>>,
    /// Names rebound more than once, or captured by a nested `def`. A pointer
    /// variable outside this set keeps one statically known loan place for its
    /// whole live range, so deref sites may substitute the owner place.
    reassigned_names: std::collections::HashSet<String>,
    returns_reference: bool,
}

/// Names whose binding may change after the first assignment (or that a nested
/// `def` captures). CFG-lowered rebindings appear as `HirInstr::Bind`; opaque
/// statements — notably `try` regions, whose sub-CFGs lower separately — are
/// scanned recursively.
fn reassigned_names(
    cfg: &Cfg,
    nested: &HashMap<String, NestedInfo>,
) -> std::collections::HashSet<String> {
    fn bump(counts: &mut HashMap<String, usize>, name: &str) {
        *counts.entry(name.to_string()).or_default() += 1;
    }
    fn scan(stmt: &Stmt, counts: &mut HashMap<String, usize>) {
        match &stmt.kind {
            StmtKind::VarDecl { name, .. }
            | StmtKind::Assign { name, .. }
            | StmtKind::RefDecl { name, .. }
            | StmtKind::Comptime { name, .. } => bump(counts, name),
            StmtKind::AugAssign { place, .. } | StmtKind::SetPlace { place, .. } => {
                if let ExprKind::Identifier(name) = &place.kind {
                    bump(counts, name);
                }
            }
            StmtKind::Unpack { targets, .. } => {
                for target in targets {
                    if let ExprKind::Identifier(name) = &target.kind {
                        bump(counts, name);
                    }
                }
            }
            StmtKind::If { branches, orelse } => {
                for (_, body) in branches {
                    for inner in body {
                        scan(inner, counts);
                    }
                }
                for inner in orelse.iter().flatten() {
                    scan(inner, counts);
                }
            }
            StmtKind::While { body, orelse, .. } => {
                for inner in body.iter().chain(orelse.iter().flatten()) {
                    scan(inner, counts);
                }
            }
            StmtKind::For {
                var, body, orelse, ..
            } => {
                bump(counts, var);
                for inner in body.iter().chain(orelse.iter().flatten()) {
                    scan(inner, counts);
                }
            }
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                let handler = except.iter().flat_map(|(_, handler)| handler.iter());
                for inner in body
                    .iter()
                    .chain(handler)
                    .chain(orelse.iter().flatten())
                    .chain(finalbody.iter().flatten())
                {
                    scan(inner, counts);
                }
            }
            _ => {}
        }
    }
    let mut counts = HashMap::new();
    for hb in cfg.g.node_indices() {
        for instr in &cfg.g[hb].instrs {
            match instr {
                HirInstr::Bind { dest, .. } => {
                    if let Some(name) = cfg.vars.get(*dest as usize) {
                        bump(&mut counts, name);
                    }
                }
                HirInstr::Stmt(statement) => scan(&statement.syntax, &mut counts),
                _ => {}
            }
        }
    }
    for info in nested.values() {
        for capture in &info.captures {
            bump(&mut counts, &capture.name);
            bump(&mut counts, &capture.name);
        }
    }
    counts
        .into_iter()
        .filter(|(_, occurrences)| *occurrences > 1)
        .map(|(name, _)| name)
        .collect()
}

impl Flatten<'_> {
    fn facts(&self, expression: &Expr) -> Option<&ExprFacts> {
        let key = expression as *const Expr as usize;
        self.active_semantics
            .iter()
            .rev()
            .find_map(|index| index.get(&key))
    }

    fn checked_ty(&self, expression: &Expr) -> Option<Ty> {
        self.facts(expression).and_then(|facts| facts.ty.clone())
    }

    fn checked_place_ty(&self, expression: &Expr) -> Option<Ty> {
        self.facts(expression)
            .and_then(|facts| facts.place_ty.clone())
    }

    fn checked_raises(&self, expression: &Expr) -> Option<Ty> {
        self.facts(expression)
            .and_then(|facts| facts.raises.clone())
    }

    fn checked_owner(&self, expression: &Expr) -> Option<crate::origin::OwnerId> {
        self.facts(expression).and_then(|facts| facts.owner)
    }

    fn comprehension_bindings(
        &self,
        expression: &Expr,
    ) -> Vec<crate::checked::CheckedComprehensionBinding> {
        self.facts(expression)
            .map(|facts| facts.comprehension_bindings.clone())
            .unwrap_or_default()
    }

    fn expression_var(&mut self, name: &str, expression: &Expr) -> VarId {
        let checked = self
            .checked_owner(expression)
            .and_then(|owner| self.owner_vars.get(&owner).copied());
        checked.unwrap_or_else(|| self.var(name))
    }
    /// Every owner loan carried into an aggregate expression.  An aggregate may
    /// contain more than one reference-valued field, so this must remain plural:
    /// keeping only the first borrow makes later fields dangling-capable.
    fn aggregate_borrows(&mut self, expression: &Expr) -> Vec<(MirPlace, bool)> {
        let borrow = self
            .checked_adjustments(expression)
            .into_iter()
            .find_map(|adjustment| match adjustment {
                crate::SemanticAdjustment::BorrowShared => Some(false),
                crate::SemanticAdjustment::BorrowMutable => Some(true),
                _ => None,
            });
        if let Some(mutable) = borrow
            && let ExprKind::Identifier(name) = &expression.kind
        {
            let var = self.expression_var(name, expression);
            return self
                .aliases
                .get(&var)
                .cloned()
                .map(|(place, _)| vec![(place, mutable)])
                .unwrap_or_default();
        }
        if let ExprKind::Identifier(name) = &expression.kind {
            let var = self.expression_var(name, expression);
            if let Some(loans) = self.aggregate_loans.get(&var) {
                return loans.clone();
            }
        }
        match &expression.kind {
            ExprKind::Call { args, kwargs, .. } => {
                // A checked pointer construction loans exactly its source
                // place, with the mutability the checker inferred from the
                // owner binding.
                if let Some(crate::SemanticAdjustment::PointerToPlace { mutable }) = self
                    .checked_adjustments(expression)
                    .into_iter()
                    .find(|adjustment| {
                        matches!(adjustment, crate::SemanticAdjustment::PointerToPlace { .. })
                    })
                {
                    let place = self.place(
                        &kwargs
                            .first()
                            .expect("checked pointer construction has a 'to=' argument")
                            .value,
                    );
                    return vec![(place, mutable)];
                }
                args.iter()
                    .chain(kwargs.iter().map(|argument| &argument.value))
                    .flat_map(|argument| self.aggregate_borrows(argument))
                    .collect()
            }
            ExprKind::Transfer(inner) => self.aggregate_borrows(inner),
            ExprKind::ListLit(values) | ExprKind::TupleLit(values) => values
                .iter()
                .flat_map(|value| self.aggregate_borrows(value))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn checked_adjustments(&self, expression: &Expr) -> Vec<crate::SemanticAdjustment> {
        self.facts(expression)
            .map(|facts| facts.adjustments.clone())
            .unwrap_or_default()
    }

    /// Whether an expression's checked type is a pointer whose provenance
    /// designates checked storage. Dereferencing such a pointer goes through
    /// its frame/slot handle instead of allocation arithmetic.
    fn is_origin_bearing_pointer(&self, expression: &Expr) -> bool {
        matches!(
            self.checked_ty(expression),
            Some(Ty::Pointer { origin, .. }) if origin.as_origin().is_some()
        )
    }

    /// The statically known storage place behind a stably bound origin-bearing
    /// pointer variable. Substituting it at deref sites touches the owner at
    /// each access — the liveness contract `ref` aliases have — so ASAP
    /// destruction and loan conflicts stay exact. A reassigned or captured
    /// pointer, or a handle loaded from a field, reads through its runtime
    /// handle instead.
    fn pointer_deref_place(&mut self, object: &Expr) -> Option<MirPlace> {
        let ExprKind::Identifier(name) = &object.kind else {
            return None;
        };
        if !self.is_origin_bearing_pointer(object) || self.reassigned_names.contains(name) {
            return None;
        }
        let var = self.expression_var(name, object);
        let loans = self.aggregate_loans.get(&var)?;
        let [(place, _)] = loans.as_slice() else {
            return None;
        };
        let mut place = place.clone();
        place.through = Some(var);
        Some(place)
    }

    fn resolved_callable(&self, expression: &Expr) -> Option<String> {
        self.checked_adjustments(expression)
            .into_iter()
            .find_map(|adjustment| match adjustment {
                crate::SemanticAdjustment::ResolveCallable(target) => Some(target),
                _ => None,
            })
    }

    fn implicit_conversion(&self, expression: &Expr) -> Option<String> {
        self.checked_adjustments(expression)
            .into_iter()
            .find_map(|adjustment| match adjustment {
                crate::SemanticAdjustment::ImplicitConversion(target) => Some(target),
                _ => None,
            })
    }

    fn is_slice_descriptor(&self, expression: &Expr) -> bool {
        matches!(
            self.checked_ty(expression),
            Some(Ty::Struct(name, args))
                if matches!(name.as_str(), "Slice" | "ContiguousSlice" | "StridedSlice")
                    && args.is_empty()
        )
    }

    fn reference_handle(&mut self, expression: &Expr) -> Reg {
        if let ExprKind::Identifier(name) = &expression.kind {
            let var = self.expression_var(name, expression);
            if let Some((place, _)) = self.aliases.get(&var).cloned() {
                let dest = self.fresh(expression.source_span(), Some(place.root));
                self.emit(MirInstr::MakeRef { dest, place });
                return dest;
            }
            if self.runtime_aliases.contains(&var) {
                let dest = self.fresh(expression.source_span(), Some(var));
                let mut place = MirPlace::root(var, self.var_types.get(&var).cloned());
                place.through = Some(var);
                self.emit(MirInstr::MakeRef { dest, place });
                return dest;
            }
        }
        if matches!(expression.kind, ExprKind::TypeApply { .. })
            && self
                .checked_adjustments(expression)
                .iter()
                .any(|adjustment| {
                    matches!(adjustment, crate::SemanticAdjustment::VariantProject { .. })
                })
        {
            let place = self.place(expression);
            let dest = self.fresh(expression.source_span(), Some(place.root));
            self.emit(MirInstr::MakeRef { dest, place });
            return dest;
        }
        if matches!(
            expression.kind,
            ExprKind::Member { .. } | ExprKind::Index { .. }
        ) {
            let place = self.place(expression);
            let storage = self.fresh(expression.source_span(), Some(place.root));
            self.emit(MirInstr::MakeRef {
                dest: storage,
                place,
            });
            let dest = self.fresh(expression.source_span(), None);
            self.emit(MirInstr::ReadRef {
                dest,
                reference: storage,
            });
            return dest;
        }
        // A reference-valued field load produces the stored frame/slot handle.
        self.expr_unconverted(expression)
    }

    fn fresh(&mut self, span: SourceSpan, origin: Option<VarId>) -> Reg {
        let r = self.next_reg;
        self.next_reg += 1;
        self.f.spans.0.insert(r, (span.clone(), origin));
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
            .unwrap_or_else(|| {
                let ty = self.var_types.get(&var).cloned();
                MirPlace::root(var, ty)
            })
    }

    fn expression_place_root(&mut self, name: &str, expression: &Expr) -> MirPlace {
        let checked_var = self
            .checked_owner(expression)
            .and_then(|owner| self.owner_vars.get(&owner).copied());
        let mut place = if let Some(var) = checked_var {
            self.aliases
                .get(&var)
                .map(|(place, _)| {
                    let mut place = place.clone();
                    place.through = Some(var);
                    place
                })
                .unwrap_or_else(|| MirPlace::root(var, self.var_types.get(&var).cloned()))
        } else {
            self.resolved_place(name)
        };
        if place.root_ty.is_none() {
            let ty = self
                .checked_place_ty(expression)
                .or_else(|| self.checked_ty(expression));
            place.root_ty = ty.clone();
            place.ty = ty.clone();
            if let Some(ty) = ty {
                self.var_types.insert(place.root, ty);
            }
        }
        place
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
            binding_ty: None,
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
            binding_ty: None,
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
            binding_ty: None,
        });
        self.f.blocks[self.cur].term = MirTerm::Jump(merge_blk);
        self.cur = else_blk;
        let re = self.expr(else_e);
        self.emit(MirInstr::DefVar {
            var: result,
            src: re,
            binding_ty: None,
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
                binding_ty: None,
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

    /// Lower comprehension clauses directly into MIR control flow. This is the
    /// same left-to-right nesting as an explicit series of `for`/`if` blocks;
    /// the final leaf performs the collection family's insertion protocol.
    fn comprehension_clauses(
        &mut self,
        clauses: &[crate::ast::ComprehensionClause],
        bindings: &[crate::checked::CheckedComprehensionBinding],
        index: usize,
        collection: VarId,
        key: Option<&Expr>,
        value: &Expr,
    ) {
        if index == clauses.len() {
            // Dictionary evaluation is key-before-value, matching an ordinary
            // display and indexed assignment. List/set leaves evaluate one item.
            let key = key.map(|expression| self.expr(expression));
            let value = self.expr(value);
            self.emit(MirInstr::CollectionInsert {
                collection,
                key,
                value,
            });
            return;
        }

        match &clauses[index] {
            crate::ast::ComprehensionClause::If(condition) => {
                let condition = self.expr(condition);
                let body = self.new_block();
                let continuation = self.new_block();
                self.f.blocks[self.cur].term = MirTerm::Branch {
                    cond: condition,
                    then_b: body,
                    else_b: continuation,
                };
                self.cur = body;
                self.comprehension_clauses(clauses, bindings, index + 1, collection, key, value);
                self.f.blocks[self.cur].term = MirTerm::Jump(continuation);
                self.cur = continuation;
            }
            crate::ast::ComprehensionClause::For {
                var, owned, iter, ..
            } => {
                let iterator_name = format!("$compiter{}", self.vars.len());
                let iterator = self.var(&iterator_name);
                let iterator_value = self.expr(iter);
                let iterator_ty = self.checked_ty(iter);
                if let Some(ty) = iterator_ty.clone() {
                    self.var_types.insert(iterator, ty);
                }
                self.emit(MirInstr::DefVar {
                    var: iterator,
                    src: iterator_value,
                    binding_ty: iterator_ty,
                });
                let protocol = self
                    .checked_adjustments(iter)
                    .into_iter()
                    .find_map(|adjustment| match adjustment {
                        crate::SemanticAdjustment::Iterate(protocol) => Some(protocol),
                        _ => None,
                    })
                    .unwrap_or(crate::IterationProtocol {
                        mode: if *owned {
                            crate::IterationMode::Owned
                        } else {
                            crate::IterationMode::Borrowed
                        },
                        prepare: Vec::new(),
                        has_next: None,
                        next: None,
                    });
                self.emit(MirInstr::GetIter {
                    iter: iterator,
                    mode: protocol.mode,
                    prepare: protocol.prepare.clone(),
                });

                let header = self.new_block();
                let body = self.new_block();
                let exit = self.new_block();
                self.f.blocks[self.cur].term = MirTerm::Jump(header);
                self.cur = header;
                let has_next = self.fresh(iter.source_span(), Some(iterator));
                self.emit(MirInstr::HasNext {
                    dest: has_next,
                    iter: iterator,
                    method: protocol.has_next.clone(),
                });
                self.f.blocks[self.cur].term = MirTerm::Branch {
                    cond: has_next,
                    then_b: body,
                    else_b: exit,
                };

                self.cur = body;
                let element_value = self.fresh(iter.source_span(), Some(iterator));
                self.emit(MirInstr::Next {
                    dest: element_value,
                    iter: iterator,
                    method: protocol.next.clone(),
                });
                let binding_index = clauses[..index]
                    .iter()
                    .filter(|clause| matches!(clause, crate::ast::ComprehensionClause::For { .. }))
                    .count();
                let binding = bindings
                    .get(binding_index)
                    .expect("checked comprehension binder metadata");
                let target = self.var(&format!("$comp{}${}", var, binding.owner.0));
                self.owner_vars.insert(binding.owner, target);
                let element_ty = Some(binding.ty.clone());
                if let Some(ty) = element_ty.clone() {
                    self.var_types.insert(target, ty);
                }
                self.emit(MirInstr::DefVar {
                    var: target,
                    src: element_value,
                    binding_ty: element_ty,
                });
                self.comprehension_clauses(clauses, bindings, index + 1, collection, key, value);
                self.f.blocks[self.cur].term = MirTerm::Jump(header);
                self.cur = exit;
            }
        }
    }

    fn comprehension(
        &mut self,
        expression: &Expr,
        kind: crate::ast::CollectionKind,
        key: Option<&Expr>,
        value: &Expr,
        clauses: &[crate::ast::ComprehensionClause],
    ) -> Reg {
        let empty = self.fresh(expression.source_span(), None);
        let result_ty = self.checked_ty(expression);
        match kind {
            crate::ast::CollectionKind::List => self.emit(MirInstr::MakeList {
                dest: empty,
                elems: Vec::new(),
                element_type: match &result_ty {
                    Some(Ty::List(element)) => Some((**element).clone()),
                    _ => None,
                },
            }),
            crate::ast::CollectionKind::Set => self.emit(MirInstr::MakeSet {
                dest: empty,
                elems: Vec::new(),
                element_type: match &result_ty {
                    Some(Ty::Set(element)) => Some((**element).clone()),
                    _ => None,
                },
            }),
            crate::ast::CollectionKind::Dict => self.emit(MirInstr::MakeDict {
                dest: empty,
                entries: Vec::new(),
                key_type: match &result_ty {
                    Some(Ty::Dict(key, _)) => Some((**key).clone()),
                    _ => None,
                },
                value_type: match &result_ty {
                    Some(Ty::Dict(_, value)) => Some((**value).clone()),
                    _ => None,
                },
            }),
        }
        let collection = self.fresh_var();
        if let Some(ty) = result_ty.clone() {
            self.var_types.insert(collection, ty);
        }
        self.emit(MirInstr::DefVar {
            var: collection,
            src: empty,
            binding_ty: result_ty,
        });
        let bindings = self.comprehension_bindings(expression);
        self.comprehension_clauses(clauses, &bindings, 0, collection, key, value);
        let result = self.fresh(expression.source_span(), Some(collection));
        self.emit(MirInstr::UseVar {
            dest: result,
            var: collection,
            mode: UseMode::Move,
        });
        result
    }

    /// Post-order: each subexpression emits one instruction and yields its result
    /// `Reg`, so `foo(bar(x))` → `t0 = bar(x); t1 = foo(t0)`. Total over `Expr`.
    fn expr_hir(&mut self, expression: &crate::hir::HirExpr) -> Reg {
        let mut index = HashMap::new();
        index_hir_expression(&expression.syntax, expression, &mut index);
        self.active_semantics.push(index);
        let result = self.expr(&expression.syntax);
        self.active_semantics.pop();
        result
    }

    fn place_hir(&mut self, expression: &crate::hir::HirExpr) -> MirPlace {
        let mut index = HashMap::new();
        index_hir_expression(&expression.syntax, expression, &mut index);
        self.active_semantics.push(index);
        let result = self.place(&expression.syntax);
        self.active_semantics.pop();
        result
    }

    fn expr(&mut self, e: &Expr) -> Reg {
        let result = self.expr_with_adjustments(e);
        if let Some(ty) = self.checked_ty(e) {
            self.f.reg_types.insert(result.0, ty);
        }
        result
    }

    fn expr_with_adjustments(&mut self, e: &Expr) -> Reg {
        if self.checked_adjustments(e).iter().any(|adjustment| {
            matches!(
                adjustment,
                crate::SemanticAdjustment::BorrowShared | crate::SemanticAdjustment::BorrowMutable
            )
        }) {
            return self.reference_handle(e);
        }
        if let Some(target) = self.implicit_conversion(e) {
            let argument = self.expr_unconverted(e);
            let dest = self.fresh(span(e), None);
            self.emit(MirInstr::Call {
                dest,
                func: FuncRef::named(&target),
                raises: None,
                args: vec![argument],
                kwargs: Vec::new(),
                arg_places: vec![None],
                param_arg_regs: Vec::new(),
            });
            return dest;
        }
        self.expr_unconverted(e)
    }

    fn expr_unconverted(&mut self, e: &Expr) -> Reg {
        match &e.kind {
            // --- Literals ------------------------------------------------------
            ExprKind::Int(n) => self.constant(e, Const::Int(*n)),
            ExprKind::Float(x) => self.constant(e, Const::Float(*x)),
            ExprKind::Bool(b) => self.constant(e, Const::Bool(*b)),
            ExprKind::Str(s) => self.constant(e, Const::Str(s.clone())),
            ExprKind::None => self.constant(e, Const::None),
            ExprKind::Uninitialized => self.constant(e, Const::None),
            ExprKind::Spread(_) => {
                let dest = self.fresh(span(e), None);
                self.emit(MirInstr::Unsupported(
                    "unexpanded call spread reached MIR lowering".to_string(),
                ));
                self.emit(MirInstr::Const {
                    dest,
                    k: Const::None,
                });
                dest
            }

            // --- Variable reads ------------------------------------------------
            // A bare read defaults to `Copy`; a call site refines it to
            // `Borrow*`/`Move` per the callee's convention (Stage 6).
            ExprKind::Identifier(name) => {
                if let Some(target) = self.resolved_callable(e) {
                    return self.constant(e, Const::Function(target));
                }
                if let Some(info) = self.nested.get(name).cloned() {
                    let captures = info
                        .captures
                        .iter()
                        .map(|capture| MirClosureCapture {
                            place: self.resolved_place(&capture.name),
                            moved: capture.kind == crate::ast::CaptureKind::Move,
                        })
                        .collect();
                    let dest = self.fresh(span(e), None);
                    self.emit(MirInstr::MakeClosure {
                        dest,
                        function: info.mangled,
                        captures,
                    });
                    return dest;
                }
                if !self.vars.iter().any(|candidate| candidate == name)
                    && self.overloads.is_function(name)
                {
                    return self.constant(e, Const::Function(name.clone()));
                }
                let var = self.expression_var(name, e);
                let d = self.fresh(span(e), Some(var));
                if self.is_origin_bearing_pointer(e) {
                    // Reading a pointer variable produces its handle value;
                    // `UseVar` would read through the stored `Value::Ref` the
                    // way a `ref` binding does. `MakeRef` on the root forwards
                    // the existing handle unchanged.
                    self.emit(MirInstr::MakeRef {
                        dest: d,
                        place: MirPlace::root(var, self.var_types.get(&var).cloned()),
                    });
                    return d;
                }
                if let Some((mut place, _)) = self.aliases.get(&var).cloned() {
                    place.through = Some(var);
                    self.emit(MirInstr::LoadPlace { dest: d, place });
                } else if self.runtime_aliases.contains(&var) {
                    let handle = self.fresh(e.source_span(), Some(var));
                    self.emit(MirInstr::MakeRef {
                        dest: handle,
                        place: {
                            let mut place = MirPlace::root(var, self.var_types.get(&var).cloned());
                            place.through = Some(var);
                            place
                        },
                    });
                    self.emit(MirInstr::ReadRef {
                        dest: d,
                        reference: handle,
                    });
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
                    let var = self.expression_var(name, inner);
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
                // A checked pointer construction materializes the frame/slot
                // handle for its source place; the checked pointer type keeps
                // the origin while the runtime value erases it.
                if self.checked_adjustments(e).iter().any(|adjustment| {
                    matches!(adjustment, crate::SemanticAdjustment::PointerToPlace { .. })
                }) {
                    let value = &kwargs
                        .first()
                        .expect("checked pointer construction has a 'to=' argument")
                        .value;
                    let place = self.place(value);
                    let dest = self.fresh(span(e), Some(place.root));
                    self.emit(MirInstr::MakeRef { dest, place });
                    return dest;
                }
                if let Some(crate::SemanticAdjustment::ConstructVariant {
                    alternatives,
                    index,
                }) = self.checked_adjustments(e).into_iter().find(|adjustment| {
                    matches!(
                        adjustment,
                        crate::SemanticAdjustment::ConstructVariant { .. }
                    )
                }) {
                    let value = self.expr(
                        args.first()
                            .expect("checked Variant construction has one payload"),
                    );
                    let dest = self.fresh(span(e), None);
                    self.emit(MirInstr::MakeVariant {
                        dest,
                        alternatives,
                        index,
                        value,
                    });
                    return dest;
                }
                // SIMD construction resolves its `[DType.<dt>, width]` parameters
                // here (the MIR is otherwise untyped about them).
                if let Some(r) = self.try_simd_call(e, args) {
                    return r;
                }
                // A call to a nested `def` (a closure, called by name in scope):
                // rewrite to its lifted function, prepending the captured enclosing
                // locals as leading arguments (passed as places, so the `mut`
                // capture parameters write back — reference-capture semantics).
                if let Some(info) = self.nested.get(name).cloned() {
                    return self.lower_nested_call(e, &info, args);
                }
                // A local with a function type (normally a callable parameter)
                // shadows any global function of the same name.
                if self.vars.iter().any(|candidate| candidate == name) {
                    let callee = self.expr(&Expr {
                        kind: ExprKind::Identifier(name.clone()),
                        span: e.span,
                        source: e.source.clone(),
                    });
                    let regs = self.args(args);
                    let kw = kwargs
                        .iter()
                        .map(|arg| (arg.name.clone(), self.expr(&arg.value)))
                        .collect();
                    let dest = self.fresh(span(e), None);
                    self.emit(MirInstr::CallIndirect {
                        dest,
                        callee,
                        raises: self.checked_raises(e),
                        args: regs,
                        kwargs: kw,
                    });
                    return dest;
                }
                // Tuple is compiler-shaped in this bounded runtime. Lower both
                // inferred `Tuple(...)` and typed `Tuple[T, ...](...)` directly
                // to aggregate construction, retaining resolved element types so
                // literal arguments materialize to an explicitly requested type.
                if name == "Tuple" && kwargs.is_empty() && !self.overloads.is_function(name) {
                    let regs = self.args(args);
                    let element_types = match self.checked_ty(e) {
                        Some(Ty::Tuple(elements)) => Some(elements),
                        _ => None,
                    };
                    let dest = self.fresh(span(e), None);
                    self.emit(MirInstr::MakeTuple {
                        dest,
                        elems: regs,
                        element_types,
                    });
                    return dest;
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
                        ParamArg::Named { value, .. } => match &**value {
                            ParamArg::Value(e) => Some(self.expr(e)),
                            ParamArg::Type(_) => None,
                            ParamArg::Named { .. } => unreachable!(),
                        },
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
                    .resolved_callable(e)
                    .unwrap_or_else(|| self.overloaded_name(name, args.len()));
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::Call {
                    dest: d,
                    func: FuncRef::named(&target),
                    raises: self.checked_raises(e),
                    args: regs,
                    kwargs: kw,
                    arg_places,
                    param_arg_regs,
                });
                d
            }
            ExprKind::Invoke {
                callee,
                param_args: _,
                args,
                kwargs,
            } => {
                if let Some(operation) =
                    self.checked_adjustments(e).into_iter().find(|adjustment| {
                        matches!(
                            adjustment,
                            crate::SemanticAdjustment::VariantIs { .. }
                                | crate::SemanticAdjustment::VariantTypeSupported { .. }
                                | crate::SemanticAdjustment::VariantSet { .. }
                                | crate::SemanticAdjustment::VariantTake { .. }
                                | crate::SemanticAdjustment::VariantReplace { .. }
                        )
                    })
                {
                    let ExprKind::Member { object, .. } = &callee.kind else {
                        unreachable!("checked Variant operation has a member callee")
                    };
                    match operation {
                        crate::SemanticAdjustment::VariantIs { index, .. } => {
                            let variant = self.expr(object);
                            let dest = self.fresh(span(e), None);
                            self.emit(MirInstr::VariantIs {
                                dest,
                                variant,
                                index,
                            });
                            return dest;
                        }
                        crate::SemanticAdjustment::VariantTypeSupported { supported } => {
                            let dest = self.fresh(span(e), None);
                            self.emit(MirInstr::Const {
                                dest,
                                k: Const::Bool(supported),
                            });
                            return dest;
                        }
                        crate::SemanticAdjustment::VariantSet { index, .. } => {
                            let place = self
                                .try_place(object)
                                .expect("checked Variant.set receiver is a writable place");
                            let value = self
                                .expr(args.first().expect("checked Variant.set has one payload"));
                            let dest = self.fresh(span(e), None);
                            self.emit(MirInstr::VariantSet {
                                dest,
                                place,
                                index,
                                value,
                            });
                            return dest;
                        }
                        crate::SemanticAdjustment::VariantTake { index, checked, .. } => {
                            let place = self
                                .try_place(object)
                                .expect("checked Variant.take receiver is an owned place");
                            let variant = self.fresh(span(object), None);
                            self.emit(MirInstr::MovePlace {
                                dest: variant,
                                place,
                            });
                            let dest = self.fresh(span(e), None);
                            self.emit(MirInstr::VariantTake {
                                dest,
                                variant,
                                index,
                                checked,
                            });
                            return dest;
                        }
                        crate::SemanticAdjustment::VariantReplace {
                            input_index,
                            output_index,
                            checked,
                            ..
                        } => {
                            let place = self
                                .try_place(object)
                                .expect("checked Variant.replace receiver is writable");
                            let value = self.expr(
                                args.first()
                                    .expect("checked Variant.replace has one payload"),
                            );
                            let dest = self.fresh(span(e), None);
                            self.emit(MirInstr::VariantReplace {
                                dest,
                                place,
                                input_index,
                                output_index,
                                value,
                                checked,
                            });
                            return dest;
                        }
                        _ => unreachable!("filtered Variant operation"),
                    }
                }
                let callee = self.expr(callee);
                let args = self.args(args);
                let kwargs = kwargs
                    .iter()
                    .map(|arg| (arg.name.clone(), self.expr(&arg.value)))
                    .collect();
                let dest = self.fresh(span(e), None);
                self.emit(MirInstr::CallIndirect {
                    dest,
                    callee,
                    raises: self.checked_raises(e),
                    args,
                    kwargs,
                });
                dest
            }
            ExprKind::MethodCall {
                object,
                method,
                args,
                kwargs,
            } => {
                let explicit_destroy = self.checked_adjustments(e).iter().any(|adjustment| {
                    matches!(adjustment, crate::SemanticAdjustment::ExplicitDestroy)
                });
                if let ExprKind::Identifier(type_name) = &object.kind
                    && !self.vars.iter().any(|name| name == type_name)
                {
                    let regs = self.args(args);
                    let kw = kwargs
                        .iter()
                        .map(|arg| (arg.name.clone(), self.expr(&arg.value)))
                        .collect();
                    let d = self.fresh(span(e), None);
                    let target = self
                        .resolved_callable(e)
                        .unwrap_or_else(|| format!("{type_name}.{method}"));
                    let arg_places = args.iter().map(|arg| self.simple_place(arg)).collect();
                    self.emit(MirInstr::Call {
                        dest: d,
                        func: FuncRef::named(&target),
                        raises: self.checked_raises(e),
                        args: regs,
                        kwargs: kw,
                        arg_places,
                        param_arg_regs: Vec::new(),
                    });
                    return d;
                }
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
                        raises: self.checked_raises(e),
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
                let receiver_expr = if explicit_destroy {
                    match &object.kind {
                        ExprKind::Transfer(inner) => inner.as_ref(),
                        _ => object.as_ref(),
                    }
                } else {
                    object.as_ref()
                };
                let (recv, recv_place) = match self.try_place(receiver_expr) {
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
                    resolved: self.resolved_callable(e),
                    raises: self.checked_raises(e),
                    args: regs,
                    kwargs: kw,
                    recv_place: if explicit_destroy { None } else { recv_place },
                    arg_places,
                });
                if explicit_destroy && let Some(place) = self.try_place(receiver_expr) {
                    if place.proj.is_empty() {
                        self.emit(MirInstr::ConsumeVar { var: place.root });
                    } else {
                        self.emit(MirInstr::ConsumePlace {
                            place,
                            marker: recv,
                        });
                    }
                }
                d
            }
            ExprKind::Member { object, field } => {
                // A pure field chain rooted at a variable (`p.a`, `p.a.b`) lowers to
                // a `LoadPlace` (a place read) so the ownership analysis sees *which*
                // field is read — enabling field-sensitive partial-move checking
                // (reading `p.b` after `p.a^` stays legal). A member of a temporary
                // or an indexed base keeps the register-based `GetField`.
                let descriptor_field = matches!(
                    self.checked_ty(object),
                    Some(Ty::Struct(name, args))
                        if matches!(name.as_str(), "Slice" | "ContiguousSlice" | "StridedSlice")
                            && args.is_empty()
                );
                if !descriptor_field && let Some(place) = self.pure_field_place(e) {
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
                // An indexed reference-bearing aggregate element is a storage
                // place whose checked type is `ref T`; load through the stored
                // handle exactly like a direct reference field.  Ordinary
                // indexing remains the register-based operation below.
                if matches!(self.checked_place_ty(e), Some(Ty::Ref(_)))
                    && let Some(place) = self.try_place(e)
                {
                    let d = self.fresh(span(e), Some(place.root));
                    self.emit(MirInstr::LoadPlace { dest: d, place });
                    return d;
                }
                // Dereferencing an origin-bearing pointer reads its source
                // place; the checker fixed the offset to 0. A stably bound
                // pointer substitutes the owner place directly, keeping the
                // owner touched (and so droppable) at each access; otherwise
                // the access reads through the runtime handle.
                if let Some(place) = self.pointer_deref_place(object) {
                    let d = self.fresh(span(e), Some(place.root));
                    self.emit(MirInstr::LoadPlace { dest: d, place });
                    return d;
                }
                if self.is_origin_bearing_pointer(object) {
                    let reference = self.expr(object);
                    let d = self.fresh(span(e), None);
                    self.emit(MirInstr::ReadRef { dest: d, reference });
                    return d;
                }
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
                let element_type = match self.checked_ty(e) {
                    Some(Ty::List(element)) => Some(*element),
                    _ => None,
                };
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::MakeList {
                    dest: d,
                    elems: regs,
                    element_type,
                });
                d
            }
            ExprKind::BraceLit(entries) => {
                let dictionary = entries.first().is_none_or(|(_, value)| value.is_some())
                    && !matches!(self.checked_ty(e), Some(Ty::Set(_)));
                let d = self.fresh(span(e), None);
                if dictionary {
                    let entries = entries
                        .iter()
                        .map(|(key, value)| {
                            let key = self.expr(key);
                            let value = self.expr(
                                value
                                    .as_ref()
                                    .expect("checked dictionary display has paired values"),
                            );
                            (key, value)
                        })
                        .collect();
                    let (key_type, value_type) = match self.checked_ty(e) {
                        Some(Ty::Dict(key, value)) => (Some(*key), Some(*value)),
                        _ => (None, None),
                    };
                    self.emit(MirInstr::MakeDict {
                        dest: d,
                        entries,
                        key_type,
                        value_type,
                    });
                } else {
                    let elems = entries.iter().map(|(key, _)| self.expr(key)).collect();
                    let element_type = match self.checked_ty(e) {
                        Some(Ty::Set(element)) => Some(*element),
                        _ => None,
                    };
                    self.emit(MirInstr::MakeSet {
                        dest: d,
                        elems,
                        element_type,
                    });
                }
                d
            }
            ExprKind::Comprehension {
                kind,
                key,
                value,
                clauses,
            } => self.comprehension(e, *kind, key.as_deref(), value, clauses),
            ExprKind::TupleLit(elems) => {
                let regs = self.args(elems);
                let element_types = match self.checked_ty(e) {
                    Some(Ty::Tuple(elements)) => Some(elements),
                    _ => None,
                };
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::MakeTuple {
                    dest: d,
                    elems: regs,
                    element_types,
                });
                d
            }

            // Walrus `:=` reaches MIR after type checking. Preserve an explicit
            // unsupported boundary rather than assigning accidental semantics.
            ExprKind::Named { name, value } => {
                let value = self.expr(value);
                let var = self.var(name);
                self.emit(MirInstr::DefVar {
                    var,
                    src: value,
                    binding_ty: None,
                });
                value
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
                ..
            } => {
                let obj = self.expr(object);
                let lower = lower.as_ref().map(|b| self.expr(b));
                let upper = upper.as_ref().map(|b| self.expr(b));
                let step = step.as_ref().map(|b| self.expr(b));
                let kind = self
                    .checked_adjustments(e)
                    .into_iter()
                    .find_map(|adjustment| match adjustment {
                        crate::SemanticAdjustment::SliceDescriptors { descriptors, .. } => {
                            descriptors.first().copied().flatten()
                        }
                        _ => None,
                    })
                    .expect("checked slice has a selected descriptor");
                let d = self.fresh(span(e), None);
                self.emit(MirInstr::Slice {
                    dest: d,
                    object: obj,
                    kind,
                    lower,
                    upper,
                    step,
                    resolved: self.resolved_callable(e),
                });
                d
            }
            ExprKind::MultiIndex { object, args } => {
                let object = self.expr(object);
                let descriptors = self
                    .checked_adjustments(e)
                    .into_iter()
                    .find_map(|adjustment| match adjustment {
                        crate::SemanticAdjustment::SliceDescriptors { descriptors, .. } => {
                            Some(descriptors)
                        }
                        _ => None,
                    })
                    .expect("checked multi-subscript has descriptor metadata");
                let args = args
                    .iter()
                    .zip(descriptors)
                    .map(|(argument, descriptor)| match argument {
                        crate::ast::SubscriptArg::Index(value) => {
                            debug_assert!(descriptor.is_none());
                            MirSubscriptArg::Index(self.expr(value))
                        }
                        crate::ast::SubscriptArg::Slice {
                            lower, upper, step, ..
                        } => MirSubscriptArg::Slice {
                            kind: descriptor.expect("slice argument has descriptor kind"),
                            lower: lower.as_ref().map(|value| self.expr(value)),
                            upper: upper.as_ref().map(|value| self.expr(value)),
                            step: step.as_ref().map(|value| self.expr(value)),
                        },
                    })
                    .collect();
                let dest = self.fresh(span(e), None);
                self.emit(MirInstr::MultiIndex {
                    dest,
                    object,
                    args,
                    resolved: self.resolved_callable(e),
                });
                dest
            }
            // These are flagged `Unsupported`/rejected by the *checker*, so a checked
            // program never reaches MIR lowering with them. A bare `TypeApply` is a
            // type used as a value (only valid as a static-method receiver, handled
            // in the `MethodCall` arm above).
            ExprKind::TString { parts, .. } => {
                let mut result = self.fresh(span(e), None);
                self.emit(MirInstr::Const {
                    dest: result,
                    k: Const::Str(String::new()),
                });
                for part in parts {
                    let piece = match part {
                        TStringPart::Literal(text) => {
                            let register = self.fresh(span(e), None);
                            self.emit(MirInstr::Const {
                                dest: register,
                                k: Const::Str(text.clone()),
                            });
                            register
                        }
                        TStringPart::Expr(value) => {
                            let argument = self.expr(value);
                            let register = self.fresh(span(value), None);
                            self.emit(MirInstr::Call {
                                dest: register,
                                func: FuncRef::named("String"),
                                raises: None,
                                args: vec![argument],
                                kwargs: Vec::new(),
                                arg_places: vec![None],
                                param_arg_regs: Vec::new(),
                            });
                            register
                        }
                    };
                    let joined = self.fresh(span(e), None);
                    self.emit(MirInstr::BinOp {
                        op: InfixOp::Add,
                        dest: joined,
                        a: result,
                        b: piece,
                    });
                    result = joined;
                }
                result
            }
            ExprKind::TypeApply { name, .. }
                if self.checked_adjustments(e).iter().any(|adjustment| {
                    matches!(adjustment, crate::SemanticAdjustment::VariantProject { .. })
                }) =>
            {
                let index = self
                    .checked_adjustments(e)
                    .into_iter()
                    .find_map(|adjustment| match adjustment {
                        crate::SemanticAdjustment::VariantProject { index, .. } => Some(index),
                        _ => None,
                    })
                    .expect("checked Variant projection carries a tag");
                let mut place = self.resolved_place(name);
                if place.root_ty.is_none() {
                    place.root_ty = Some(Ty::Variant(
                        self.checked_adjustments(e)
                            .into_iter()
                            .find_map(|adjustment| match adjustment {
                                crate::SemanticAdjustment::VariantProject {
                                    alternatives, ..
                                } => Some(alternatives),
                                _ => None,
                            })
                            .unwrap_or_default(),
                    ));
                }
                let ty = self
                    .checked_place_ty(e)
                    .or_else(|| self.checked_ty(e))
                    .expect("checked Variant projection has a payload type");
                place.project(Proj::Variant(index), ty);
                let dest = self.fresh(span(e), Some(place.root));
                self.emit(MirInstr::LoadPlace { dest, place });
                dest
            }
            ExprKind::TypeValue(_) | ExprKind::TypeApply { .. } => {
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
    fn try_simd_call(&mut self, e: &Expr, args: &[Expr]) -> Option<Reg> {
        let (dtype, width) = self
            .checked_adjustments(e)
            .into_iter()
            .find_map(|adjustment| match adjustment {
                crate::SemanticAdjustment::ConstructSimd { dtype, width } => {
                    usize::try_from(width).ok().map(|width| (dtype, width))
                }
                _ => None,
            })?;
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

    /// Lower a call to a nested `def` through the same closure-environment path as
    /// a first-class closure value. This preserves reference handles across sibling
    /// calls and recursion; it does not rely on call-return write-back.
    fn lower_nested_call(&mut self, e: &Expr, info: &NestedInfo, args: &[Expr]) -> Reg {
        let captures = info
            .captures
            .iter()
            .map(|capture| MirClosureCapture {
                place: self.resolved_place(&capture.name),
                moved: capture.kind == crate::ast::CaptureKind::Move,
            })
            .collect();
        let callee = self.fresh(span(e), None);
        self.emit(MirInstr::MakeClosure {
            dest: callee,
            function: info.mangled.clone(),
            captures,
        });
        let arg_regs = self.args(args);
        let d = self.fresh(span(e), None);
        self.emit(MirInstr::CallIndirect {
            dest: d,
            callee,
            raises: self.checked_raises(e),
            args: arg_regs,
            kwargs: Vec::new(),
        });
        for capture in &info.captures {
            if capture.kind != crate::ast::CaptureKind::Move {
                let var = self.var(&capture.name);
                self.emit(MirInstr::KeepAlive { var });
            }
        }
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
                binding_ty,
            } => {
                let mut index = HashMap::new();
                index_hir_expression(&expr.syntax, expr, &mut index);
                self.active_semantics.push(index);
                let src = self.expr(&expr.syntax);
                if let Some(ty) = expr.ty.clone().or_else(|| binding_ty.clone()) {
                    self.var_types.insert(*dest, ty);
                }
                if let Some((mut place, _)) = self.aliases.get(dest).cloned() {
                    place.through = Some(*dest);
                    self.emit(MirInstr::Store { place, src });
                } else if self.runtime_aliases.contains(dest) {
                    let handle = self.fresh(expr.source_span(), Some(*dest));
                    self.emit(MirInstr::MakeRef {
                        dest: handle,
                        place: {
                            let mut place = MirPlace::root(*dest, expr.ty.clone());
                            place.through = Some(*dest);
                            place
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
                        binding_ty: binding_ty.clone(),
                    });
                    let aggregate_loans = self.aggregate_borrows(expr);
                    for (place, mutable) in &aggregate_loans {
                        let marker = self.fresh(expr.source_span(), Some(place.root));
                        self.emit(MirInstr::BeginLoan {
                            reference: *dest,
                            place: place.clone(),
                            mutable: *mutable,
                            marker,
                        });
                    }
                    if aggregate_loans.is_empty() {
                        self.aggregate_loans.remove(dest);
                    } else {
                        self.aggregate_loans.insert(*dest, aggregate_loans);
                    }
                }
                self.active_semantics.pop();
            }
            HirInstr::Eval(e) => {
                let _ = self.expr_hir(e); // evaluated for its effect; result discarded
            }
            HirInstr::Stmt(s) => self.lower_hir_stmt(s, outer_map),
            // A `try` whose enclosing loops are function-level: lower each sub-region
            // seeded with those loops (`loop_targets`, HIR function block ids), so an
            // outward `break`/`continue` becomes an `EscapeJump` resolved via
            // `outer_map`.
            HirInstr::Try { stmt, loop_targets } => {
                let mut index = HashMap::new();
                for (syntax, expression) in statement_expression_roots(&stmt.syntax)
                    .into_iter()
                    .zip(&stmt.expressions)
                {
                    index_hir_expression(syntax, expression, &mut index);
                }
                self.active_semantics.push(index);
                if let StmtKind::Try {
                    body,
                    except,
                    orelse,
                    finalbody,
                } = &stmt.syntax.kind
                {
                    self.emit_try(body, except, orelse, finalbody, loop_targets, outer_map);
                } else {
                    self.emit(MirInstr::Unsupported(
                        "malformed HIR try instruction".to_string(),
                    ));
                }
                self.active_semantics.pop();
            }
            HirInstr::Drop(var) => {
                self.emit(MirInstr::DropVar { var: *var });
            }
            // Iterator protocol: compute into a register, then store to the target
            // variable (so the header's branch can read `has_next` as a `UseVar`,
            // and the body binds the loop variable).
            HirInstr::GetIter { iter, protocol } => {
                self.emit(MirInstr::GetIter {
                    iter: *iter,
                    mode: protocol.mode,
                    prepare: protocol.prepare.clone(),
                });
            }
            HirInstr::HasNext { iter, dest, method } => {
                let r = self.fresh(SourceSpan::new(None, DUMMY_SPAN), None);
                self.emit(MirInstr::HasNext {
                    dest: r,
                    iter: *iter,
                    method: method.clone(),
                });
                self.emit(MirInstr::DefVar {
                    var: *dest,
                    src: r,
                    binding_ty: None,
                });
            }
            HirInstr::Next { iter, dest, method } => {
                let r = self.fresh(SourceSpan::new(None, DUMMY_SPAN), Some(*iter));
                self.emit(MirInstr::Next {
                    dest: r,
                    iter: *iter,
                    method: method.clone(),
                });
                self.emit(MirInstr::DefVar {
                    var: *dest,
                    src: r,
                    binding_ty: None,
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
            ExprKind::Identifier(name) => self.expression_place_root(name, e),
            ExprKind::Member { object, field } => {
                let mut p = self.place(object);
                if let Some(ty) = self.checked_place_ty(e).or_else(|| self.checked_ty(e)) {
                    p.project(Proj::Field(field.clone()), ty);
                } else {
                    p.proj.push(Proj::Field(field.clone()));
                }
                p
            }
            ExprKind::Index { object, index } => {
                let mut p = self.place(object);
                let idx = self.expr(index); // evaluated once, before the store
                if let Some(ty) = self.checked_place_ty(e).or_else(|| self.checked_ty(e)) {
                    p.project(Proj::Index(idx), ty);
                } else {
                    p.proj.push(Proj::Index(idx));
                }
                p
            }
            ExprKind::TypeApply { name, .. } => {
                let index = self
                    .checked_adjustments(e)
                    .into_iter()
                    .find_map(|adjustment| match adjustment {
                        crate::SemanticAdjustment::VariantProject { index, .. } => Some(index),
                        _ => None,
                    })
                    .expect("only checked Variant projection is a place TypeApply");
                let mut p = self.resolved_place(name);
                let ty = self
                    .checked_place_ty(e)
                    .or_else(|| self.checked_ty(e))
                    .expect("checked Variant projection has a payload type");
                p.project(Proj::Variant(index), ty);
                p
            }
            other => {
                self.emit(MirInstr::Unsupported(format!(
                    "invalid assignment place reached MIR lowering: {other:?}"
                )));
                MirPlace::root(self.var("$invalid_place"), None)
            }
        }
    }

    /// Lower a slice or multidimensional assignment through the checker-selected
    /// `__setitem__` implementation. Unlike an ordinary `MirPlace` projection,
    /// every slice remains a first-class descriptor argument and the receiver
    /// place is retained for `mut self` write-back.
    fn lower_subscript_set(&mut self, target: &Expr, value: Reg) -> bool {
        let (object, source_arguments): (&Expr, Option<&[crate::ast::SubscriptArg]>) =
            match &target.kind {
                ExprKind::Slice { object, .. } => (object, None),
                ExprKind::MultiIndex { object, args } => (object, Some(args)),
                _ => return false,
            };
        let Some((descriptors, value_keyword)) = self
            .checked_adjustments(target)
            .into_iter()
            .find_map(|adjustment| match adjustment {
                crate::SemanticAdjustment::SliceDescriptors {
                    descriptors,
                    set_value_keyword,
                } => Some((descriptors, set_value_keyword)),
                _ => None,
            })
        else {
            self.emit(MirInstr::Unsupported(
                "checked subscript assignment lacks descriptor metadata".to_string(),
            ));
            return true;
        };

        let args = if let Some(arguments) = source_arguments {
            arguments
                .iter()
                .zip(descriptors)
                .map(|(argument, descriptor)| match argument {
                    crate::ast::SubscriptArg::Index(value) => {
                        debug_assert!(descriptor.is_none());
                        MirSubscriptArg::Index(self.expr(value))
                    }
                    crate::ast::SubscriptArg::Slice {
                        lower, upper, step, ..
                    } => MirSubscriptArg::Slice {
                        kind: descriptor.expect("slice assignment argument has descriptor kind"),
                        lower: lower.as_ref().map(|bound| self.expr(bound)),
                        upper: upper.as_ref().map(|bound| self.expr(bound)),
                        step: step.as_ref().map(|bound| self.expr(bound)),
                    },
                })
                .collect()
        } else {
            let ExprKind::Slice {
                lower, upper, step, ..
            } = &target.kind
            else {
                unreachable!("single descriptor assignment is a Slice")
            };
            vec![MirSubscriptArg::Slice {
                kind: descriptors
                    .first()
                    .copied()
                    .flatten()
                    .expect("slice assignment has descriptor kind"),
                lower: lower.as_ref().map(|bound| self.expr(bound)),
                upper: upper.as_ref().map(|bound| self.expr(bound)),
                step: step.as_ref().map(|bound| self.expr(bound)),
            }]
        };
        let receiver_place = self.place(object);
        self.emit(MirInstr::MultiSet {
            receiver_place,
            args,
            value,
            value_keyword,
            resolved: self.resolved_callable(target),
        });
        true
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
        let checked: Vec<_> = self.checked_expressions.values().cloned().collect();
        let region_cfg =
            hir::Cfg::build_seeded_checked_with_loops(self.vars.clone(), body, ext_loops, &checked);
        let mut region = MirFunction {
            blocks: Vec::new(),
            n_regs: 0,
            n_vars: 0,
            var_names: Vec::new(),
            n_params: 0,
            param_types: Vec::new(),
            owned_params: Vec::new(),
            ref_params: Vec::new(),
            returns_reference: self.returns_reference,
            spans: std::mem::take(&mut self.f.spans), // accumulate into the shared table
            reg_types: std::mem::take(&mut self.f.reg_types),
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
                var_types: region_cfg.var_types.clone(),
                owner_vars: self.owner_vars.clone(),
                nested: self.nested.clone(), // a `try` region may call a nested `def`
                overloads: self.overloads.clone(),
                checked_expressions: self.checked_expressions.clone(),
                active_semantics: Vec::new(),
                aliases: self.aliases.clone(),
                runtime_aliases: self.runtime_aliases.clone(),
                aggregate_loans: self.aggregate_loans.clone(),
                reassigned_names: self.reassigned_names.clone(),
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
            self.var_types = fl.var_types.clone();
            self.owner_vars = fl.owner_vars.clone();
        }
        self.f.spans = std::mem::take(&mut region.spans);
        self.f.reg_types = std::mem::take(&mut region.reg_types);
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
            ExprKind::Identifier(name) => Some(self.expression_place_root(name, e)),
            ExprKind::Member { object, field } => {
                if self.is_slice_descriptor(object) {
                    return None;
                }
                let mut p = self.simple_place(object)?;
                if let Some(ty) = self.checked_place_ty(e).or_else(|| self.checked_ty(e)) {
                    p.project(Proj::Field(field.clone()), ty);
                } else {
                    p.proj.push(Proj::Field(field.clone()));
                }
                Some(p)
            }
            ExprKind::TypeApply { name, .. } => {
                let index = self
                    .checked_adjustments(e)
                    .into_iter()
                    .find_map(|adjustment| match adjustment {
                        crate::SemanticAdjustment::VariantProject { index, .. } => Some(index),
                        _ => None,
                    })?;
                let mut p = self.resolved_place(name);
                let ty = self.checked_place_ty(e).or_else(|| self.checked_ty(e))?;
                p.project(Proj::Variant(index), ty);
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
                Some(self.expression_place_root(root, e))
            }
            ExprKind::Member { object, field } => {
                if self.is_slice_descriptor(object) {
                    return None;
                }
                let mut p = self.pure_field_place(object)?;
                if let Some(ty) = self.checked_place_ty(e).or_else(|| self.checked_ty(e)) {
                    p.project(Proj::Field(field.clone()), ty);
                } else {
                    p.proj.push(Proj::Field(field.clone()));
                }
                Some(p)
            }
            _ => None,
        }
    }

    fn try_place(&mut self, e: &Expr) -> Option<MirPlace> {
        match &e.kind {
            ExprKind::Identifier(name) => Some(self.expression_place_root(name, e)),
            ExprKind::Member { object, field } => {
                if self.is_slice_descriptor(object) {
                    return None;
                }
                let mut p = self.try_place(object)?;
                if let Some(ty) = self.checked_place_ty(e).or_else(|| self.checked_ty(e)) {
                    p.project(Proj::Field(field.clone()), ty);
                } else {
                    p.proj.push(Proj::Field(field.clone()));
                }
                Some(p)
            }
            ExprKind::Index { object, index } => {
                let mut p = self.try_place(object)?;
                let idx = self.expr(index);
                if let Some(ty) = self.checked_place_ty(e).or_else(|| self.checked_ty(e)) {
                    p.project(Proj::Index(idx), ty);
                } else {
                    p.proj.push(Proj::Index(idx));
                }
                Some(p)
            }
            ExprKind::TypeApply { name, .. } => {
                let index = self
                    .checked_adjustments(e)
                    .into_iter()
                    .find_map(|adjustment| match adjustment {
                        crate::SemanticAdjustment::VariantProject { index, .. } => Some(index),
                        _ => None,
                    })?;
                let mut p = self.resolved_place(name);
                let ty = self.checked_place_ty(e).or_else(|| self.checked_ty(e))?;
                p.project(Proj::Variant(index), ty);
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
                    ExprKind::Identifier(_)
                        | ExprKind::Member { .. }
                        | ExprKind::Index { .. }
                        | ExprKind::TypeApply { .. }
                ) {
                    let source = self.expr(value);
                    self.runtime_aliases.insert(reference);
                    self.emit(MirInstr::DefVar {
                        var: reference,
                        src: source,
                        binding_ty: None,
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
                if self.lower_subscript_set(place, src) {
                    return;
                }
                // A store through an origin-bearing pointer writes its source
                // place; the checker fixed the offset to 0 and required
                // mutable provenance. A stably bound pointer substitutes the
                // owner place; otherwise the store goes through the handle.
                if let ExprKind::Index { object, .. } = &place.kind {
                    if let Some(target) = self.pointer_deref_place(object) {
                        self.emit(MirInstr::Store { place: target, src });
                        return;
                    }
                    if self.is_origin_bearing_pointer(object) {
                        let reference = self.expr(object);
                        self.emit(MirInstr::WriteRef {
                            reference,
                            value: src,
                        });
                        return;
                    }
                }
                let p = self.place(place);
                let stores_reference = matches!(p.ty, Some(Ty::Ref(_)))
                    && self.checked_adjustments(value).iter().any(|adjustment| {
                        matches!(
                            adjustment,
                            crate::SemanticAdjustment::BorrowShared
                                | crate::SemanticAdjustment::BorrowMutable
                        )
                    });
                if stores_reference {
                    self.emit(MirInstr::StoreRef {
                        place: p,
                        reference: src,
                    });
                } else {
                    self.emit(MirInstr::Store { place: p, src });
                }
            }
            StmtKind::AugAssign { place, op, value } => {
                // `place OP= e` — read the place, apply the op, write it back. A bare
                // variable uses the `UseVar`/`DefVar` fast path (what move-analysis
                // reads for a var); a projected place uses `LoadPlace`/`Store`, with
                // the place flattened once so its indices are evaluated once.
                if let ExprKind::Identifier(name) = &place.kind {
                    let var = self.var(name);
                    let cur = self.expr(place);
                    let rhs = self.expr(value);
                    let res = self.fresh(span(place), None);
                    self.emit(MirInstr::BinOp {
                        op: *op,
                        dest: res,
                        a: cur,
                        b: rhs,
                    });
                    if self.runtime_aliases.contains(&var) {
                        let handle = self.fresh(place.source_span(), Some(var));
                        self.emit(MirInstr::MakeRef {
                            dest: handle,
                            place: {
                                let mut place =
                                    MirPlace::root(var, self.var_types.get(&var).cloned());
                                place.through = Some(var);
                                place
                            },
                        });
                        self.emit(MirInstr::WriteRef {
                            reference: handle,
                            value: res,
                        });
                    } else if let Some((mut target, _)) = self.aliases.get(&var).cloned() {
                        target.through = Some(var);
                        self.emit(MirInstr::Store {
                            place: target,
                            src: res,
                        });
                    } else {
                        self.emit(MirInstr::DefVar {
                            var,
                            src: res,
                            binding_ty: None,
                        });
                    }
                } else if let ExprKind::Index { object, .. } = &place.kind
                    && let Some(target) = self.pointer_deref_place(object)
                {
                    // `p[0] OP= e` through a stably bound pointer: owner-place
                    // load and store, exactly like an alias write-back.
                    let cur = self.fresh(span(place), Some(target.root));
                    self.emit(MirInstr::LoadPlace {
                        dest: cur,
                        place: target.clone(),
                    });
                    let rhs = self.expr(value);
                    let res = self.fresh(span(place), None);
                    self.emit(MirInstr::BinOp {
                        op: *op,
                        dest: res,
                        a: cur,
                        b: rhs,
                    });
                    self.emit(MirInstr::Store {
                        place: target,
                        src: res,
                    });
                } else if let ExprKind::Index { object, .. } = &place.kind
                    && self.is_origin_bearing_pointer(object)
                {
                    // `p[0] OP= e` through an origin-bearing pointer: read and
                    // write through the handle, evaluated once.
                    let reference = self.expr(object);
                    let cur = self.fresh(span(place), None);
                    self.emit(MirInstr::ReadRef {
                        dest: cur,
                        reference,
                    });
                    let rhs = self.expr(value);
                    let res = self.fresh(span(place), None);
                    self.emit(MirInstr::BinOp {
                        op: *op,
                        dest: res,
                        a: cur,
                        b: rhs,
                    });
                    self.emit(MirInstr::WriteRef {
                        reference,
                        value: res,
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
                    binding_ty: None,
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
            // A nested `def` we couldn't lift because it nests another declaration,
            // or a nested `struct`/`trait`, stays a clean `Unsupported`.
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
                                binding_ty: None,
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

    fn lower_hir_stmt(
        &mut self,
        statement: &crate::hir::HirStmt,
        outer_map: &HashMap<hir::BlockId, MirBlockId>,
    ) {
        let mut index = HashMap::new();
        for (syntax, expression) in statement_expression_roots(&statement.syntax)
            .into_iter()
            .zip(&statement.expressions)
        {
            index_hir_expression(syntax, expression, &mut index);
        }
        self.active_semantics.push(index);
        self.lower_stmt(&statement.syntax, outer_map);
        self.active_semantics.pop();
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
                let c = self.expr_hir(cond); // evaluated at the end of this block
                MirTerm::Branch {
                    cond: c,
                    then_b: map[then_b],
                    else_b: map[else_b],
                }
            }
            Terminator::Return(e) => MirTerm::Return(e.as_ref().map(|e| {
                if self.returns_reference {
                    let place = self.place_hir(e);
                    let dest = self.fresh(e.source_span(), Some(place.root));
                    self.emit(MirInstr::MakeRef { dest, place });
                    dest
                } else {
                    self.expr_hir(e)
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
        false,
        &[],
    )
}

/// [`lower_cfg`] with a nested-`def` registry in scope: a call to a registered
/// nested `def` is rewritten to its lifted function (captures prepended) and the
/// nested `def` statement lowers to nothing.
fn lower_cfg_nested(
    cfg: &Cfg,
    nested: &HashMap<String, NestedInfo>,
    overloads: &crate::symbol::OverloadSets,
    returns_reference: bool,
    reference_parameters: &[bool],
) -> MirFunction {
    let mut mir = MirFunction {
        blocks: Vec::new(),
        n_regs: 0,
        n_vars: cfg.vars.len(),
        var_names: cfg.vars.clone(),
        n_params: cfg.n_params,
        param_types: Vec::new(),
        owned_params: Vec::new(),
        ref_params: Vec::new(),
        returns_reference,
        spans: SpanTable::default(),
        reg_types: HashMap::new(),
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
            var_types: cfg.var_types.clone(),
            owner_vars: HashMap::new(),
            nested: nested.clone(),
            overloads: overloads.clone(),
            checked_expressions: cfg.checked_expressions.clone(),
            active_semantics: Vec::new(),
            aliases: HashMap::new(),
            runtime_aliases: reference_parameters
                .iter()
                .take(cfg.n_params)
                .enumerate()
                .filter_map(|(slot, reference)| reference.then_some(slot as VarId))
                .collect(),
            aggregate_loans: HashMap::new(),
            reassigned_names: reassigned_names(cfg, nested),
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
/// `__toplevel__` holds module initialization and explicit legacy test snippets.
/// Production compilation rejects executable file-scope source statements.
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
    pub explicit_destroy_message: Option<String>,
    pub explicit_destructors: HashMap<String, bool>,
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

/// Translate a source parameter marker into the runtime frame layout. Named
/// `out` results are callee-local slots, so they do not consume an incoming
/// argument position; variadic collectors do consume one frame position.
fn runtime_parameter_index(params: &[FnParam], marker: Option<usize>) -> Option<usize> {
    marker.map(|index| {
        params[..index]
            .iter()
            .filter(|parameter| {
                !matches!(parameter.convention, Some(ArgConvention::Out))
                    && parameter.kind != ParamKind::KwVariadic
            })
            .count()
    })
}

/// `*args` is inserted among regular incoming arguments before the collector is
/// materialized, so only preceding non-`out` regular parameters determine its
/// insertion point.
fn runtime_variadic_index(params: &[FnParam], marker: Option<usize>) -> Option<usize> {
    marker.map(|index| {
        params[..index]
            .iter()
            .filter(|parameter| {
                parameter.kind == ParamKind::Regular
                    && !matches!(parameter.convention, Some(ArgConvention::Out))
            })
            .count()
    })
}

mod nested;
use nested::*;

/// Lower a whole program (a top-level statement list) into per-function MIR.
///
/// Decision — **declarations are handled here, not inside a function body**: each
/// top-level `def` becomes its own `MirFunction`; each `struct` method becomes
/// `Struct.method`; a `trait`'s bodiless requirements produce nothing (default
/// methods are deferred). Remaining statements form `__toplevel__`; production
/// compilation has already rejected executable file-scope source statements.
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
                let named_result = params
                    .iter()
                    .find(|p| matches!(p.convention, Some(ArgConvention::Out)));
                // ABI parameters lead the variable table; the named result is a
                // callee-local uninitialized slot and is never passed by callers.
                let caller_params: Vec<_> = params
                    .iter()
                    .filter(|p| !matches!(p.convention, Some(ArgConvention::Out)))
                    .collect();
                let mut names: Vec<String> = caller_params.iter().map(|p| p.name.clone()).collect();
                if let Some(result) = named_result {
                    names.push(result.name.clone());
                }
                let ptys = caller_params
                    .iter()
                    .map(|p| {
                        let param = params
                            .iter()
                            .position(|candidate| std::ptr::eq(candidate, *p))
                            .expect("caller parameter belongs to declaration");
                        checked_type_or_record(
                            checked,
                            AnnotationSite::FunctionParam {
                                module: s.module.clone(),
                                declaration: s.span,
                                param,
                            },
                            &format!("parameter '{}' of function '{name}'", p.name),
                            &mut invariant_errors,
                        )
                    })
                    .collect();
                let owned = caller_params
                    .iter()
                    .map(|p| is_owned(&p.convention))
                    .collect();
                let refp = caller_params
                    .iter()
                    .map(|p| is_ref(&p.convention))
                    .collect();
                let lowered_name =
                    crate::symbol::lowered_def_name(name, type_params, params, &overloads);
                let variadic_idx = params.iter().position(|p| p.kind == ParamKind::Variadic);
                let kw_variadic_idx = params.iter().position(|p| p.kind == ParamKind::KwVariadic);
                let regular: Vec<_> = params
                    .iter()
                    .filter(|p| {
                        p.kind == ParamKind::Regular
                            && !matches!(p.convention, Some(ArgConvention::Out))
                    })
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
                    variadic_index: runtime_variadic_index(params, variadic_idx),
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
                    kw_variadic_index: runtime_parameter_index(params, kw_variadic_idx),
                    positional_only: regular_marker_index(params, *positional_only),
                    keyword_only: effective_keyword_only_index(params, *keyword_only, variadic_idx),
                    param_decls: crate::runtime::classify_param_decls(type_params),
                });
                lower_fn_nested(
                    FunctionLowering {
                        checked,
                        name: &lowered_name,
                        parameter_names: &names,
                        parameter_types: ptys,
                        owned_parameters: owned,
                        reference_parameters: refp,
                        returns_reference: matches!(ret, Some(SourceType::Ref { .. })),
                        named_result: named_result.map(|p| p.name.as_str()),
                        body,
                        overloads: &overloads,
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
                            m.self_convention,
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
                    explicit_destroy_message: checked
                        .explicit_destroy_types()
                        .get(name)
                        .map(|info| info.message.clone()),
                    explicit_destructors: checked
                        .explicit_destroy_types()
                        .get(name)
                        .map(|info| info.destructors.clone())
                        .unwrap_or_default(),
                });
                for (method_index, m) in methods.iter().enumerate() {
                    let method_name = crate::symbol::lifecycle_method_name(m);
                    let source_mangled = format!("{name}.{method_name}");
                    let mangled = crate::symbol::lowered_method_name(
                        &source_mangled,
                        type_params,
                        &m.params,
                        m.self_convention,
                        &overloads,
                    );
                    let variadic_idx = m
                        .params
                        .iter()
                        .position(|param| param.kind == ParamKind::Variadic);
                    let kw_variadic_idx = m
                        .params
                        .iter()
                        .position(|param| param.kind == ParamKind::KwVariadic);
                    let regular: Vec<_> = m
                        .params
                        .iter()
                        .filter(|param| {
                            param.kind == ParamKind::Regular
                                && !matches!(param.convention, Some(ArgConvention::Out))
                        })
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
                        variadic_index: runtime_variadic_index(&m.params, variadic_idx),
                        kw_variadic: kw_variadic_idx.map(|index| {
                            checked_type_or_record(
                                checked,
                                AnnotationSite::MethodParam {
                                    module: s.module.clone(),
                                    declaration: s.span,
                                    method: method_index,
                                    param: index,
                                },
                                &format!("keyword variadic parameter of method '{source_mangled}'"),
                                &mut invariant_errors,
                            )
                        }),
                        kw_variadic_index: runtime_parameter_index(&m.params, kw_variadic_idx),
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
                    let mut ptys: Vec<Ty> = Vec::new();
                    let mut owned: Vec<bool> = Vec::new();
                    let mut refp: Vec<bool> = Vec::new();
                    if m.has_self {
                        names.push("self".to_string());
                        ptys.push(Ty::Struct(name.clone(), Vec::new()));
                        owned.push(is_owned(&m.self_convention));
                        refp.push(is_ref(&m.self_convention));
                    }
                    names.extend(m.params.iter().map(|p| p.name.clone()));
                    ptys.extend(m.params.iter().enumerate().map(|(param, p)| {
                        checked_type_or_record(
                            checked,
                            AnnotationSite::MethodParam {
                                module: s.module.clone(),
                                declaration: s.span,
                                method: method_index,
                                param,
                            },
                            &format!("parameter '{}' of method '{source_mangled}'", p.name),
                            &mut invariant_errors,
                        )
                    }));
                    owned.extend(m.params.iter().map(|p| is_owned(&p.convention)));
                    refp.extend(m.params.iter().map(|p| is_ref(&p.convention)));
                    lower_fn_nested(
                        FunctionLowering {
                            checked,
                            name: &mangled,
                            parameter_names: &names,
                            parameter_types: ptys,
                            owned_parameters: owned,
                            reference_parameters: refp,
                            returns_reference: matches!(m.ret, Some(SourceType::Ref { .. })),
                            named_result: None,
                            body: &m.body,
                            overloads: &overloads,
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
            &Cfg::build_checked_fn(checked, &[], &toplevel),
            &HashMap::new(),
            &overloads,
            false,
            &[],
        ),
    ));
    let mut result = MirProgram {
        functions,
        declarations,
        invariant_errors,
    };
    result.invariant_errors.extend(verify::verify(&result));
    result
}
