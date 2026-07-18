//! Stage 4: lower the checked AST into a **control-flow graph**
//! (CFG) of basic blocks, built on `petgraph`. Expressions stay *nested* here (as
//! embedded AST); Stage 5 flattens them to A-Normal Form. Ownership and liveness
//! analysis (Stages 6–7) run over the flattened form, not this one.
//!
//! A [`Cfg`] is built per function body (or the top-level statement list) via
//! [`Cfg::build`]. Every block ends in exactly one [`Terminator`]; control flow
//! (`if`/`while`/`for`/`break`/`continue`/`return`) becomes blocks + edges, so
//! later passes are pure graph traversal.

use crate::ast::{Expr, ExprKind, Stmt, StmtKind};
use crate::token::{DUMMY_SPAN, SourceSpan};
use crate::{
    CheckedExpr, CheckedNodeId, CheckedProgram, EffectFacts, SemanticAdjustment, Ty, ValueCategory,
};
use petgraph::stable_graph::{NodeIndex, StableGraph};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct HirExpr {
    pub syntax: Expr,
    pub checked: Option<CheckedNodeId>,
    pub ty: Option<Ty>,
    pub category: ValueCategory,
    /// Checker-selected effect contract for this expression. Calls through a
    /// trait bound retain the requirement's typed error here just like direct
    /// and indirect calls.
    pub effects: EffectFacts,
    pub adjustments: Vec<SemanticAdjustment>,
    /// Recursively checked children in the same structural order as `syntax`.
    /// Downstream passes use these identities rather than recovering facts by
    /// comparing source spans.
    pub children: Vec<HirExpr>,
    /// Fully checked place shape when this expression denotes storage. This is
    /// independent of runtime `VarId` assignment and survives name shadowing.
    pub place: Option<HirPlace>,
    /// Stable generator binders introduced by a comprehension, in source
    /// `for`-clause order. Ordinary expressions leave this empty.
    pub comprehension_bindings: Vec<crate::checked::CheckedComprehensionBinding>,
}

#[derive(Debug, Clone)]
pub struct HirPlace {
    pub owner: crate::origin::OwnerId,
    pub root_ty: Ty,
    pub projections: Vec<HirPlaceProjection>,
    pub ty: Ty,
}

#[derive(Debug, Clone)]
pub struct HirPlaceProjection {
    pub kind: HirPlaceProjectionKind,
    pub base_ty: Ty,
    pub ty: Ty,
}

#[derive(Debug, Clone)]
pub enum HirPlaceProjectionKind {
    Field(String),
    Index(CheckedNodeId),
    Variant(usize),
}

impl HirExpr {
    fn unchecked(syntax: Expr) -> Self {
        Self {
            syntax,
            checked: None,
            ty: None,
            category: ValueCategory::Value,
            effects: EffectFacts::default(),
            adjustments: Vec::new(),
            children: Vec::new(),
            place: None,
            comprehension_bindings: Vec::new(),
        }
    }
}

impl std::ops::Deref for HirExpr {
    type Target = Expr;
    fn deref(&self) -> &Self::Target {
        &self.syntax
    }
}

#[derive(Debug, Clone)]
pub struct HirStmt {
    pub syntax: Stmt,
    /// Checked roots directly owned by this opaque statement, in source/AST
    /// order. Nested control-flow regions build their own CFGs.
    pub expressions: Vec<HirExpr>,
}

fn explicit_local_names(body: &[Stmt]) -> HashSet<String> {
    let mut names = HashSet::new();
    fn walk(body: &[Stmt], names: &mut HashSet<String>) {
        for statement in body {
            match &statement.kind {
                StmtKind::VarDecl { name, .. } | StmtKind::RefDecl { name, .. } => {
                    names.insert(name.clone());
                }
                StmtKind::If { branches, orelse } => {
                    for (_, body) in branches {
                        walk(body, names);
                    }
                    if let Some(body) = orelse {
                        walk(body, names);
                    }
                }
                StmtKind::While { body, .. }
                | StmtKind::For { body, .. }
                | StmtKind::With { body, .. } => walk(body, names),
                StmtKind::Try {
                    body,
                    except,
                    orelse,
                    finalbody,
                } => {
                    walk(body, names);
                    if let Some((_, body)) = except {
                        walk(body, names);
                    }
                    if let Some(body) = orelse {
                        walk(body, names);
                    }
                    if let Some(body) = finalbody {
                        walk(body, names);
                    }
                }
                StmtKind::Def { .. } | StmtKind::Struct { .. } | StmtKind::Trait { .. } => {}
                _ => {}
            }
        }
    }
    walk(body, &mut names);
    names
}

fn collect_named_expr(expression: &Expr, names: &mut HashSet<String>) {
    match &expression.kind {
        ExprKind::Named { name, value } => {
            names.insert(name.clone());
            collect_named_expr(value, names);
        }
        ExprKind::Prefix(_, value) | ExprKind::Transfer(value) => collect_named_expr(value, names),
        ExprKind::Infix(_, left, right)
        | ExprKind::Index {
            object: left,
            index: right,
        } => {
            collect_named_expr(left, names);
            collect_named_expr(right, names);
        }
        ExprKind::Call { args, kwargs, .. } => {
            for argument in args {
                collect_named_expr(argument, names);
            }
            for argument in kwargs {
                collect_named_expr(&argument.value, names);
            }
        }
        ExprKind::Invoke {
            callee,
            args,
            kwargs,
            ..
        } => {
            collect_named_expr(callee, names);
            for argument in args {
                collect_named_expr(argument, names);
            }
            for argument in kwargs {
                collect_named_expr(&argument.value, names);
            }
        }
        ExprKind::Member { object, .. } => collect_named_expr(object, names),
        ExprKind::MethodCall {
            object,
            args,
            kwargs,
            ..
        } => {
            collect_named_expr(object, names);
            for argument in args {
                collect_named_expr(argument, names);
            }
            for argument in kwargs {
                collect_named_expr(&argument.value, names);
            }
        }
        ExprKind::Slice {
            object,
            lower,
            upper,
            step,
            ..
        } => {
            collect_named_expr(object, names);
            for bound in [lower, upper, step].into_iter().flatten() {
                collect_named_expr(bound, names);
            }
        }
        ExprKind::MultiIndex { object, args } => {
            collect_named_expr(object, names);
            for argument in args {
                match argument {
                    crate::ast::SubscriptArg::Index(value) => collect_named_expr(value, names),
                    crate::ast::SubscriptArg::Slice {
                        lower, upper, step, ..
                    } => {
                        for value in [lower, upper, step].into_iter().flatten() {
                            collect_named_expr(value, names);
                        }
                    }
                }
            }
        }
        ExprKind::ListLit(values) | ExprKind::TupleLit(values) => {
            for value in values {
                collect_named_expr(value, names);
            }
        }
        ExprKind::IfExpr {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_named_expr(cond, names);
            collect_named_expr(then_branch, names);
            collect_named_expr(else_branch, names);
        }
        ExprKind::Compare { first, rest } => {
            collect_named_expr(first, names);
            for (_, value) in rest {
                collect_named_expr(value, names);
            }
        }
        ExprKind::TString { parts, .. } => {
            for part in parts {
                if let crate::ast::TStringPart::Expr(value) = part {
                    collect_named_expr(value, names);
                }
            }
        }
        _ => {}
    }
}

fn collect_function_implicit_names(
    body: &[Stmt],
    explicit: &HashSet<String>,
    names: &mut HashSet<String>,
) {
    for statement in body {
        match &statement.kind {
            StmtKind::Assign { name, value } => {
                if !explicit.contains(name) {
                    names.insert(name.clone());
                }
                collect_named_expr(value, names);
            }
            StmtKind::VarDecl { value, .. }
            | StmtKind::RefDecl { value, .. }
            | StmtKind::Comptime { value, .. }
            | StmtKind::Raise(value)
            | StmtKind::Expr(value) => collect_named_expr(value, names),
            StmtKind::Return(Some(value)) => collect_named_expr(value, names),
            StmtKind::If { branches, orelse } => {
                for (condition, body) in branches {
                    collect_named_expr(condition, names);
                    collect_function_implicit_names(body, explicit, names);
                }
                if let Some(body) = orelse {
                    collect_function_implicit_names(body, explicit, names);
                }
            }
            StmtKind::While { cond, body, orelse } => {
                collect_named_expr(cond, names);
                collect_function_implicit_names(body, explicit, names);
                if let Some(body) = orelse {
                    collect_function_implicit_names(body, explicit, names);
                }
            }
            StmtKind::For {
                iter, body, orelse, ..
            } => {
                collect_named_expr(iter, names);
                collect_function_implicit_names(body, explicit, names);
                if let Some(body) = orelse {
                    collect_function_implicit_names(body, explicit, names);
                }
            }
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                collect_function_implicit_names(body, explicit, names);
                if let Some((_, body)) = except {
                    collect_function_implicit_names(body, explicit, names);
                }
                if let Some(body) = orelse {
                    collect_function_implicit_names(body, explicit, names);
                }
                if let Some(body) = finalbody {
                    collect_function_implicit_names(body, explicit, names);
                }
            }
            StmtKind::Unpack { targets, value } => {
                for target in targets {
                    collect_named_expr(target, names);
                }
                collect_named_expr(value, names);
            }
            StmtKind::SetPlace { place, value } | StmtKind::AugAssign { place, value, .. } => {
                collect_named_expr(place, names);
                collect_named_expr(value, names);
            }
            StmtKind::With { items, body } => {
                for item in items {
                    collect_named_expr(&item.context, names);
                }
                collect_function_implicit_names(body, explicit, names);
            }
            StmtKind::Def { .. } | StmtKind::Struct { .. } | StmtKind::Trait { .. } => {}
            _ => {}
        }
    }
}

pub type BlockId = NodeIndex;
pub type VarId = u32;

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
        // Structured statements and declarations are either decomposed by HIR or
        // rebuilt as independently checked nested CFGs; their nested expressions
        // are not roots of this opaque instruction.
        _ => Vec::new(),
    }
}

fn rename_expr(e: &mut Expr, resolve: &impl Fn(&str) -> String) {
    match &mut e.kind {
        ExprKind::Identifier(n) => *n = resolve(n),
        ExprKind::Prefix(_, x) | ExprKind::Transfer(x) | ExprKind::Spread(x) => {
            rename_expr(x, resolve)
        }
        ExprKind::Infix(_, l, r) => {
            rename_expr(l, resolve);
            rename_expr(r, resolve);
        }
        ExprKind::Compare { first, rest } => {
            rename_expr(first, resolve);
            for (_, x) in rest {
                rename_expr(x, resolve);
            }
        }
        ExprKind::Call { args, kwargs, .. } => {
            for x in args {
                rename_expr(x, resolve);
            }
            for x in kwargs {
                rename_expr(&mut x.value, resolve);
            }
        }
        ExprKind::Member { object, .. } => rename_expr(object, resolve),
        ExprKind::MethodCall {
            object,
            args,
            kwargs,
            ..
        } => {
            rename_expr(object, resolve);
            for x in args {
                rename_expr(x, resolve);
            }
            for x in kwargs {
                rename_expr(&mut x.value, resolve);
            }
        }
        ExprKind::Index { object, index } => {
            rename_expr(object, resolve);
            rename_expr(index, resolve);
        }
        ExprKind::Slice {
            object,
            lower,
            upper,
            step,
            ..
        } => {
            rename_expr(object, resolve);
            for x in [lower, upper, step].into_iter().flatten() {
                rename_expr(x, resolve);
            }
        }
        ExprKind::MultiIndex { object, args } => {
            rename_expr(object, resolve);
            for argument in args {
                match argument {
                    crate::ast::SubscriptArg::Index(value) => rename_expr(value, resolve),
                    crate::ast::SubscriptArg::Slice {
                        lower, upper, step, ..
                    } => {
                        for value in [lower, upper, step].into_iter().flatten() {
                            rename_expr(value, resolve);
                        }
                    }
                }
            }
        }
        ExprKind::ListLit(xs) | ExprKind::TupleLit(xs) => {
            for x in xs {
                rename_expr(x, resolve);
            }
        }
        ExprKind::BraceLit(entries) => {
            for (key, value) in entries {
                rename_expr(key, resolve);
                if let Some(value) = value {
                    rename_expr(value, resolve);
                }
            }
        }
        ExprKind::Comprehension {
            key,
            value,
            clauses,
            ..
        } => {
            if let Some(key) = key {
                rename_expr(key, resolve);
            }
            rename_expr(value, resolve);
            for clause in clauses {
                match clause {
                    crate::ast::ComprehensionClause::For { iter, .. } => rename_expr(iter, resolve),
                    crate::ast::ComprehensionClause::If(condition) => {
                        rename_expr(condition, resolve)
                    }
                }
            }
        }
        ExprKind::Invoke {
            callee,
            args,
            kwargs,
            ..
        } => {
            rename_expr(callee, resolve);
            for arg in args {
                rename_expr(arg, resolve);
            }
            for arg in kwargs {
                rename_expr(&mut arg.value, resolve);
            }
        }
        ExprKind::Named { name, value } => {
            *name = resolve(name);
            rename_expr(value, resolve);
        }
        ExprKind::IfExpr {
            cond,
            then_branch,
            else_branch,
        } => {
            rename_expr(cond, resolve);
            rename_expr(then_branch, resolve);
            rename_expr(else_branch, resolve);
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::None
        | ExprKind::Uninitialized
        | ExprKind::TypeValue(_)
        | ExprKind::TString { .. }
        | ExprKind::TypeApply { .. } => {}
    }
}

/// One straight-line instruction inside a basic block. Control flow lives in the
/// block's [`Terminator`], never here.
#[derive(Debug, Clone)]
// HIR intentionally retains complete checked expressions/statements until MIR
// makes their evaluation order explicit; boxing every transitional payload would
// obscure that handoff without reducing the retained data.
#[allow(clippy::large_enum_variant)]
pub enum HirInstr {
    /// A variable definition (`var x = e`, or a reassignment): a dataflow *def*.
    /// `binding_ty` is the resolved checked type for an initializing binding.
    /// Reassignments use `None` and retain the existing slot type.
    Bind {
        dest: VarId,
        expr: HirExpr,
        binding_ty: Option<Ty>,
    },
    /// A bare expression evaluated for its effect/value (`f(x)`).
    Eval(HirExpr),
    /// Any other straight-line statement (assignment, member-write, declaration,
    /// …), kept whole for Stage 4 and refined when Stage 5 flattens to MIR.
    Stmt(HirStmt),
    /// An ASAP destructor, spliced in by Stage 7 liveness (never by the Stage 4
    /// lowerer — present so later stages share the type).
    Drop(VarId),
    /// Iterator protocol: normalize the iterable in `iter` to an *iterator* — for a
    /// user struct, `iter = iter.__iter__()`; a built-in `range`/`List` iterates in
    /// place, so this is a no-op. Emitted once before the loop header.
    GetIter {
        iter: VarId,
        protocol: crate::IterationProtocol,
    },
    /// Iterator protocol (`for` loops): `dest = whether `iter` yields another
    /// element` (a `Bool`), a pure read of the iterator's state. `iter`/`dest` are
    /// variable slots so both IRs and the backend address them uniformly.
    HasNext {
        iter: VarId,
        dest: VarId,
        method: Option<String>,
    },
    /// Iterator protocol: `dest = iter.next()` — bind the current element and
    /// advance `iter` in place (a mutating read of the iterator).
    Next {
        iter: VarId,
        dest: VarId,
        method: Option<String>,
    },
    /// A `try` statement whose sub-regions are lowered in Stage 5 as mini-CFGs.
    /// `loop_targets` snapshots the enclosing **function-level** loop stack
    /// (`(continue → header, break → exit)`, innermost-last) at this point, so a
    /// `break`/`continue` inside the `try` that targets an outer loop can be
    /// resolved to that loop's block (an `EscapeJump`). Only produced when every
    /// enclosing loop is function-level; otherwise the `try` stays a `Stmt`.
    Try {
        stmt: HirStmt,
        loop_targets: Vec<(BlockId, BlockId)>,
    },
}

/// How a block hands control to its successor(s). Exactly one per block.
#[derive(Debug, Clone)]
pub enum Terminator {
    Jump(BlockId),
    Branch {
        cond: HirExpr,
        then_b: BlockId,
        else_b: BlockId,
    },
    Return(Option<HirExpr>),
    /// The normal fall-through end of a **seeded region** (a `try` sub-body). Unlike
    /// `Return(None)` (an explicit bare `return`), this means "the region completed
    /// normally" — the VM continues to the `else`/`finally`, not out of the frame.
    /// Only appears in region CFGs (`build_seeded`), never a function body.
    FallOff,
    /// A `break`/`continue` inside a seeded region that targets a loop in the
    /// **enclosing function** CFG: `target` is that loop's exit (`break`) or header
    /// (`continue`) block, an id in the enclosing CFG, *not* this region's. No graph
    /// edge is added (the target isn't a node here); the VM propagates it out as a
    /// `Flow::Jump`. Only appears in region CFGs seeded with external loops.
    EscapeJump(BlockId),
}

#[derive(Debug, Default)]
pub struct BasicBlock {
    pub instrs: Vec<HirInstr>,
    pub term: Option<Terminator>,
}

/// A control-flow graph for one function body / statement list.
#[derive(Debug)]
pub struct Cfg {
    pub g: StableGraph<BasicBlock, ()>,
    pub entry: BlockId,
    /// The variable interner table: `vars[id as usize]` is the name that `id` was
    /// assigned. Exposed so the Stage 5 MIR flattener can seed its own interner and
    /// keep `VarId`s consistent across the two IRs.
    pub vars: Vec<String>,
    /// The number of leading `vars` that are the function's **parameters**, in
    /// declaration order (so `vars[0..n_params]` are the params). The call ABI: a
    /// caller binds its argument values to these var slots. `0` for the top-level
    /// block and for bodies built without a parameter list.
    pub n_params: usize,
    /// Typed semantic nodes available to region lowering. Kept by stable node id;
    /// source locations are used only once, when AST syntax enters checked HIR.
    pub checked_expressions: HashMap<CheckedNodeId, CheckedExpr>,
    /// Checked storage types for variable slots known at HIR construction.
    pub var_types: HashMap<VarId, Ty>,
}

impl Cfg {
    /// Lower a statement sequence (the top-level block, or a parameterless body)
    /// into a CFG. The final fall-through block is given an implicit `return None`.
    pub fn build(body: &[Stmt]) -> Cfg {
        Cfg::build_fn(&[], body)
    }

    /// Lower a function body whose `params` (in declaration order) seed the
    /// variable interner **first**, so `VarId`s `0..params.len()` are the
    /// parameters — the call ABI the MIR/VM rely on to bind arguments. The final
    /// fall-through block gets an implicit `return None`.
    pub fn build_fn(params: &[String], body: &[Stmt]) -> Cfg {
        Self::build_fn_with_captures(params, HashSet::new(), body)
    }

    pub fn build_checked_fn(checked: &CheckedProgram, params: &[String], body: &[Stmt]) -> Cfg {
        Self::build_fn_with_context(params, HashSet::new(), body, checked.expressions())
    }

    pub(crate) fn build_fn_with_captures(
        params: &[String],
        shadow_captures: HashSet<String>,
        body: &[Stmt],
    ) -> Cfg {
        Self::build_fn_with_context(params, shadow_captures, body, &[])
    }

    pub(crate) fn build_checked_fn_with_captures(
        checked: &CheckedProgram,
        params: &[String],
        shadow_captures: HashSet<String>,
        body: &[Stmt],
    ) -> Cfg {
        Self::build_fn_with_context(params, shadow_captures, body, checked.expressions())
    }

    fn build_fn_with_context(
        params: &[String],
        shadow_captures: HashSet<String>,
        body: &[Stmt],
        checked: &[CheckedExpr],
    ) -> Cfg {
        let mut g = StableGraph::new();
        let entry = g.add_node(BasicBlock::default());
        let explicit = explicit_local_names(body);
        let mut implicit = HashSet::new();
        collect_function_implicit_names(body, &explicit, &mut implicit);
        let mut vars = params.to_vec();
        let mut root_scope: HashMap<String, String> =
            params.iter().map(|n| (n.clone(), n.clone())).collect();
        let mut implicit: Vec<_> = implicit.into_iter().collect();
        implicit.sort();
        for name in implicit {
            if !root_scope.contains_key(&name) {
                vars.push(name.clone());
                root_scope.insert(name.clone(), name);
            }
        }
        let mut lower = Lower {
            g,
            cur: entry,
            loops: Vec::new(),
            vars,
            scopes: vec![root_scope],
            captures: shadow_captures,
            is_function: true,
            checked_by_span: checked_index(checked),
            checked_expressions: checked.iter().cloned().map(|n| (n.id, n)).collect(),
        };
        for s in body {
            lower.stmt(s);
        }
        lower.seal(Terminator::Return(None)); // implicit `return None` off the end
        let var_types = checked_var_types(&lower.vars, &lower.checked_expressions);
        Cfg {
            g: lower.g,
            entry,
            vars: lower.vars,
            n_params: params.len(),
            checked_expressions: lower.checked_expressions,
            var_types,
        }
    }

    /// Lower a statement sequence into a CFG whose variable interner **starts**
    /// seeded with `seed_vars` (so a nested region shares the enclosing frame's
    /// `VarId`s — a name already interned keeps its id, new names append). Used to
    /// lower a `try` region as a self-contained mini-CFG that still addresses the
    /// same variable slots. `n_params` is 0 (a region takes no parameters).
    pub fn build_seeded(seed_vars: Vec<String>, body: &[Stmt]) -> Cfg {
        Cfg::build_seeded_with_loops(seed_vars, body, &[])
    }

    /// Like [`build_seeded`], but the region's loop stack is initialized with the
    /// enclosing function's loops (`external_loops`, innermost-last, as `(header,
    /// exit)` block ids in the *enclosing* CFG). A `break`/`continue` that targets
    /// one of them — rather than a loop declared inside the region — lowers to an
    /// [`Terminator::EscapeJump`] carrying the enclosing block id.
    pub fn build_seeded_with_loops(
        seed_vars: Vec<String>,
        body: &[Stmt],
        external_loops: &[(BlockId, BlockId)],
    ) -> Cfg {
        Self::build_seeded_checked_with_loops(seed_vars, body, external_loops, &[])
    }

    pub fn build_seeded_checked_with_loops(
        seed_vars: Vec<String>,
        body: &[Stmt],
        external_loops: &[(BlockId, BlockId)],
        checked: &[CheckedExpr],
    ) -> Cfg {
        let mut g = StableGraph::new();
        let entry = g.add_node(BasicBlock::default());
        let loops = external_loops
            .iter()
            .map(|&(header, exit)| LoopFrame {
                header,
                exit,
                escape: true,
            })
            .collect();
        let mut lower = Lower {
            g,
            cur: entry,
            loops,
            scopes: vec![seed_vars.iter().map(|n| (n.clone(), n.clone())).collect()],
            captures: HashSet::new(),
            vars: seed_vars,
            is_function: false,
            checked_by_span: checked_index(checked),
            checked_expressions: checked.iter().cloned().map(|n| (n.id, n)).collect(),
        };
        for s in body {
            lower.stmt(s);
        }
        lower.seal(Terminator::FallOff); // region completed normally (not a return)
        let var_types = checked_var_types(&lower.vars, &lower.checked_expressions);
        Cfg {
            g: lower.g,
            entry,
            vars: lower.vars,
            n_params: 0,
            checked_expressions: lower.checked_expressions,
            var_types,
        }
    }

    // --- Read-only accessors (so tests/analysis need not import `petgraph`) ---

    pub fn node_count(&self) -> usize {
        self.g.node_count()
    }
    pub fn edge_count(&self) -> usize {
        self.g.edge_count()
    }
    pub fn block(&self, b: BlockId) -> &BasicBlock {
        &self.g[b]
    }
    pub fn term(&self, b: BlockId) -> Option<&Terminator> {
        self.g[b].term.as_ref()
    }
    /// The successor blocks of `b`, by graph edge (order unspecified).
    pub fn successors(&self, b: BlockId) -> Vec<BlockId> {
        self.g.neighbors(b).collect()
    }
    /// Whether an edge `from → to` exists.
    pub fn has_edge(&self, from: BlockId, to: BlockId) -> bool {
        self.g.neighbors(from).any(|n| n == to)
    }
}

/// One enclosing loop's `break`/`continue` targets. `escape = false` for a loop
/// in *this* CFG (a `break`/`continue` is a `Jump`); `escape = true` for a loop in
/// the enclosing **function** CFG (seeded into a `try` region — a `break`/
/// `continue` becomes an `EscapeJump` the VM propagates out).
#[derive(Clone, Copy)]
struct LoopFrame {
    header: BlockId,
    exit: BlockId,
    escape: bool,
}

/// Lowering cursor: appends to the "current" block, splitting on control flow.
/// Invariant: `cur` is the current insertion point; a block gets its terminator
/// (and out-edges) exactly once, via [`Lower::seal`].
struct Lower {
    g: StableGraph<BasicBlock, ()>,
    cur: BlockId,
    /// Innermost-last stack of enclosing loops' targets.
    loops: Vec<LoopFrame>,
    /// Interner: a variable name's first appearance assigns its `VarId`.
    vars: Vec<String>,
    scopes: Vec<HashMap<String, String>>,
    captures: HashSet<String>,
    /// Whether this is a function-body CFG (vs. a seeded `try` region). A
    /// function's own loops are function-level, so a `try` inside one can escape to
    /// them; a region's own loops are region-local and cannot be escape targets.
    is_function: bool,
    checked_by_span: HashMap<SourceSpan, Vec<CheckedNodeId>>,
    checked_expressions: HashMap<CheckedNodeId, CheckedExpr>,
}

fn checked_index(nodes: &[CheckedExpr]) -> HashMap<SourceSpan, Vec<CheckedNodeId>> {
    let mut result: HashMap<SourceSpan, Vec<CheckedNodeId>> = HashMap::new();
    for node in nodes {
        result
            .entry(node.syntax.source_span())
            .or_default()
            .push(node.id);
    }
    result
}

fn checked_var_types(
    vars: &[String],
    nodes: &HashMap<CheckedNodeId, CheckedExpr>,
) -> HashMap<VarId, Ty> {
    vars.iter()
        .enumerate()
        .filter_map(|(slot, runtime_name)| {
            let source_name = runtime_name.split("$shadow").next().unwrap_or(runtime_name);
            let ty = nodes.values().find_map(|node| match &node.syntax.kind {
                ExprKind::Identifier(name) if name == source_name => {
                    node.place_ty.clone().or_else(|| node.ty.clone())
                }
                _ => None,
            })?;
            Some((slot as VarId, ty))
        })
        .collect()
}

impl Lower {
    fn checked_place(&self, node: &CheckedExpr) -> Option<HirPlace> {
        match &node.syntax.kind {
            ExprKind::Identifier(_) => Some(HirPlace {
                owner: node.binding?,
                root_ty: node.place_ty.clone()?,
                projections: Vec::new(),
                ty: node.place_ty.clone()?,
            }),
            ExprKind::Member { field, .. } => {
                let base = self.checked_expressions.get(node.children.first()?)?;
                let mut place = self.checked_place(base)?;
                let ty = node.place_ty.clone()?;
                let base_ty = place.ty.clone();
                place.projections.push(HirPlaceProjection {
                    kind: HirPlaceProjectionKind::Field(field.clone()),
                    base_ty,
                    ty: ty.clone(),
                });
                place.ty = ty;
                Some(place)
            }
            ExprKind::Index { .. } => {
                let base = self.checked_expressions.get(node.children.first()?)?;
                let index = *node.children.get(1)?;
                let mut place = self.checked_place(base)?;
                let ty = node.place_ty.clone()?;
                let base_ty = place.ty.clone();
                place.projections.push(HirPlaceProjection {
                    kind: HirPlaceProjectionKind::Index(index),
                    base_ty,
                    ty: ty.clone(),
                });
                place.ty = ty;
                Some(place)
            }
            ExprKind::TypeApply { .. } => {
                let (alternatives, index) =
                    node.adjustments
                        .iter()
                        .find_map(|adjustment| match adjustment {
                            SemanticAdjustment::VariantProject {
                                alternatives,
                                index,
                            } => Some((alternatives, *index)),
                            _ => None,
                        })?;
                let ty = node.place_ty.clone()?;
                Some(HirPlace {
                    owner: node.binding?,
                    root_ty: Ty::Variant(alternatives.clone()),
                    projections: vec![HirPlaceProjection {
                        kind: HirPlaceProjectionKind::Variant(index),
                        base_ty: Ty::Variant(alternatives.clone()),
                        ty: ty.clone(),
                    }],
                    ty,
                })
            }
            _ => None,
        }
    }

    fn new_block(&mut self) -> BlockId {
        self.g.add_node(BasicBlock::default())
    }

    fn push(&mut self, i: HirInstr) {
        // Dead code after a terminator (e.g. a statement following `return`) is
        // dropped rather than appended to an already-sealed block.
        if self.g[self.cur].term.is_none() {
            self.g[self.cur].instrs.push(i);
        }
    }

    /// Give `cur` its terminator and wire the implied out-edges — but only if it
    /// isn't already sealed (a `break`/`continue`/`return` inside a body may have
    /// sealed it first, in which case this is a no-op).
    fn seal(&mut self, t: Terminator) {
        if self.g[self.cur].term.is_some() {
            return;
        }
        match &t {
            Terminator::Jump(to) => {
                self.g.add_edge(self.cur, *to, ());
            }
            Terminator::Branch { then_b, else_b, .. } => {
                self.g.add_edge(self.cur, *then_b, ());
                self.g.add_edge(self.cur, *else_b, ());
            }
            // No in-graph edge: `Return`/`FallOff` leave the CFG; `EscapeJump`
            // targets a block in the *enclosing* CFG (not a node here).
            Terminator::Return(_) | Terminator::FallOff | Terminator::EscapeJump(_) => {}
        }
        self.g[self.cur].term = Some(t);
    }

    /// Intern a variable name to a stable `VarId`.
    fn var(&mut self, name: &str) -> VarId {
        if let Some(i) = self.vars.iter().position(|n| n == name) {
            i as VarId
        } else {
            self.vars.push(name.to_string());
            (self.vars.len() - 1) as VarId
        }
    }

    fn resolved(&self, name: &str) -> String {
        self.scopes
            .iter()
            .rev()
            .find_map(|s| s.get(name))
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    fn declare_var(&mut self, name: &str) -> VarId {
        let runtime = if self
            .scopes
            .iter()
            .rev()
            .skip(1)
            .any(|s| s.contains_key(name))
        {
            format!("{name}$shadow{}", self.vars.len())
        } else {
            name.to_string()
        };
        if self.scopes.is_empty() {
            self.scopes.push(HashMap::new());
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), runtime.clone());
        }
        self.var(&runtime)
    }

    fn expr(&self, e: &Expr) -> HirExpr {
        let original_span = e.source_span();
        let mut syntax = e.clone();
        rename_expr(&mut syntax, &|n| self.resolved(n));
        let checked = self
            .checked_by_span
            .get(&original_span)
            .and_then(|ids| ids.first())
            .and_then(|id| self.checked_expressions.get(id));
        if let Some(node) = checked {
            HirExpr {
                syntax,
                checked: Some(node.id),
                ty: node.ty.clone(),
                category: node.category,
                effects: node.effects.clone(),
                adjustments: node.adjustments.clone(),
                children: node
                    .children
                    .iter()
                    .filter_map(|id| self.checked_expressions.get(id))
                    .map(|child| self.checked_expr(child))
                    .collect(),
                place: self.checked_place(node),
                comprehension_bindings: node.comprehension_bindings.clone(),
            }
        } else {
            HirExpr::unchecked(syntax)
        }
    }

    fn checked_expr(&self, node: &CheckedExpr) -> HirExpr {
        HirExpr {
            syntax: node.syntax.clone(),
            checked: Some(node.id),
            ty: node.ty.clone(),
            category: node.category,
            effects: node.effects.clone(),
            adjustments: node.adjustments.clone(),
            children: node
                .children
                .iter()
                .filter_map(|id| self.checked_expressions.get(id))
                .map(|child| self.checked_expr(child))
                .collect(),
            place: self.checked_place(node),
            comprehension_bindings: node.comprehension_bindings.clone(),
        }
    }

    fn statement(&self, syntax: Stmt) -> HirStmt {
        let expressions = statement_expression_roots(&syntax)
            .into_iter()
            .map(|expression| self.expr(expression))
            .collect();
        HirStmt {
            syntax,
            expressions,
        }
    }

    fn scoped_block(&mut self, body: &[Stmt]) {
        self.scopes.push(HashMap::new());
        self.block(body);
        self.scopes.pop();
    }

    fn block(&mut self, body: &[Stmt]) {
        for s in body {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::If { branches, orelse } => {
                let join = self.new_block();
                for (cond, body) in branches {
                    let then_b = self.new_block();
                    let else_b = self.new_block();
                    self.seal(Terminator::Branch {
                        cond: self.expr(cond),
                        then_b,
                        else_b,
                    });
                    self.cur = then_b;
                    self.scoped_block(body);
                    self.seal(Terminator::Jump(join));
                    self.cur = else_b; // the next elif/else lowers into this block
                }
                if let Some(body) = orelse {
                    self.scoped_block(body);
                }
                self.seal(Terminator::Jump(join));
                self.cur = join;
            }

            StmtKind::While { cond, body, orelse } => {
                let header = self.new_block();
                let body_b = self.new_block();
                let exit = self.new_block();
                let normal_exit = orelse.as_ref().map(|_| self.new_block()).unwrap_or(exit);
                self.seal(Terminator::Jump(header));
                self.cur = header;
                self.seal(Terminator::Branch {
                    cond: self.expr(cond),
                    then_b: body_b,
                    else_b: normal_exit,
                });
                self.cur = body_b;
                self.loops.push(LoopFrame {
                    header,
                    exit,
                    escape: false,
                });
                self.block(body);
                self.loops.pop();
                self.seal(Terminator::Jump(header)); // back-edge (unless body sealed via break/return)
                if let Some(body) = orelse {
                    self.cur = normal_exit;
                    self.scoped_block(body);
                    self.seal(Terminator::Jump(exit));
                }
                self.cur = exit;
            }

            // `for x in it: body` — a while-shaped CFG driving the iterator
            // protocol: a pre-header evaluates the iterable once into an iterator
            // slot; the header computes `has_next` and branches on it; the body
            // binds the loop variable via `next` (which advances the iterator).
            StmtKind::For {
                var,
                reference,
                owned,
                iter,
                body,
                orelse,
            } => {
                if *reference {
                    let ExprKind::Identifier(source_name) = &iter.kind else {
                        self.push(HirInstr::Stmt(
                            self.statement(Stmt::new(StmtKind::Pass, s.span)),
                        ));
                        return;
                    };
                    let source_name = source_name.clone();
                    let index_name = format!("$refindex{}", self.vars.len());
                    let index_var = self.var(&index_name);
                    self.push(HirInstr::Bind {
                        dest: index_var,
                        expr: HirExpr::unchecked(Expr::new(ExprKind::Int(0), DUMMY_SPAN)),
                        binding_ty: None,
                    });
                    let header = self.new_block();
                    let body_b = self.new_block();
                    let exit = self.new_block();
                    let normal_exit = orelse.as_ref().map(|_| self.new_block()).unwrap_or(exit);
                    self.seal(Terminator::Jump(header));
                    self.cur = header;
                    let length = Expr::new(
                        ExprKind::Call {
                            name: "len".to_string(),
                            param_args: Vec::new(),
                            args: vec![Expr::new(
                                ExprKind::Identifier(source_name.clone()),
                                DUMMY_SPAN,
                            )],
                            kwargs: Vec::new(),
                        },
                        DUMMY_SPAN,
                    );
                    self.seal(Terminator::Branch {
                        cond: HirExpr::unchecked(Expr::new(
                            ExprKind::Infix(
                                crate::ast::InfixOp::Lt,
                                Box::new(Expr::new(
                                    ExprKind::Identifier(index_name.clone()),
                                    DUMMY_SPAN,
                                )),
                                Box::new(length),
                            ),
                            DUMMY_SPAN,
                        )),
                        then_b: body_b,
                        else_b: normal_exit,
                    });
                    self.cur = body_b;
                    let target = Expr::new(
                        ExprKind::Index {
                            object: Box::new(Expr::new(
                                ExprKind::Identifier(source_name),
                                DUMMY_SPAN,
                            )),
                            index: Box::new(Expr::new(
                                ExprKind::Identifier(index_name.clone()),
                                DUMMY_SPAN,
                            )),
                        },
                        DUMMY_SPAN,
                    );
                    self.stmt(&Stmt::new(
                        StmtKind::RefDecl {
                            name: var.clone(),
                            value: target,
                        },
                        s.span,
                    ));
                    self.loops.push(LoopFrame {
                        header,
                        exit,
                        escape: false,
                    });
                    self.block(body);
                    self.loops.pop();
                    let next = Expr::new(
                        ExprKind::Infix(
                            crate::ast::InfixOp::Add,
                            Box::new(Expr::new(
                                ExprKind::Identifier(index_name.clone()),
                                DUMMY_SPAN,
                            )),
                            Box::new(Expr::new(ExprKind::Int(1), DUMMY_SPAN)),
                        ),
                        DUMMY_SPAN,
                    );
                    self.push(HirInstr::Bind {
                        dest: index_var,
                        expr: HirExpr::unchecked(next),
                        binding_ty: None,
                    });
                    self.seal(Terminator::Jump(header));
                    if let Some(body) = orelse {
                        self.cur = normal_exit;
                        self.scoped_block(body);
                        self.seal(Terminator::Jump(exit));
                    }
                    self.cur = exit;
                    return;
                }
                // Evaluate the iterable once into a fresh iterator variable. `$`
                // names can't be produced by the parser, so they never collide with
                // user variables; a monotone `vars.len()` suffix keeps them unique.
                let it_name = format!("$iter{}", self.vars.len());
                let it_var = self.var(&it_name);
                let checked_iter = self.expr(iter);
                let protocol = checked_iter
                    .adjustments
                    .iter()
                    .find_map(|adjustment| match adjustment {
                        SemanticAdjustment::Iterate(protocol) => Some(protocol.clone()),
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
                self.push(HirInstr::Bind {
                    dest: it_var,
                    expr: checked_iter,
                    binding_ty: None,
                });
                // Normalize the iterable to an iterator (a user struct's `__iter__`;
                // a no-op for a built-in `range`/`List`).
                self.push(HirInstr::GetIter {
                    iter: it_var,
                    protocol: protocol.clone(),
                });

                let header = self.new_block();
                let body_b = self.new_block();
                let exit = self.new_block();
                let normal_exit = orelse.as_ref().map(|_| self.new_block()).unwrap_or(exit);
                self.seal(Terminator::Jump(header));

                // header: has_next(it) → branch.
                self.cur = header;
                let hn_name = format!("$hasnext{}", self.vars.len());
                let hn_var = self.var(&hn_name);
                self.push(HirInstr::HasNext {
                    iter: it_var,
                    dest: hn_var,
                    method: protocol.has_next.clone(),
                });
                self.seal(Terminator::Branch {
                    cond: HirExpr::unchecked(Expr::new(ExprKind::Identifier(hn_name), DUMMY_SPAN)),
                    then_b: body_b,
                    else_b: normal_exit,
                });

                // body: x = next(it); <body>; back-edge to header.
                self.cur = body_b;
                let v = self.var(var);
                self.push(HirInstr::Next {
                    iter: it_var,
                    dest: v,
                    method: protocol.next.clone(),
                });
                self.loops.push(LoopFrame {
                    header,
                    exit,
                    escape: false,
                });
                self.block(body);
                self.loops.pop();
                self.seal(Terminator::Jump(header));
                if let Some(body) = orelse {
                    self.cur = normal_exit;
                    self.scoped_block(body);
                    self.seal(Terminator::Jump(exit));
                }
                self.cur = exit;
            }

            StmtKind::Break => {
                let Some(f) = self.loops.last().copied() else {
                    self.push(HirInstr::Stmt(self.statement(s.clone())));
                    return;
                };
                // A loop in this CFG → a plain `Jump` to its exit; an enclosing
                // function loop (seeded into a `try` region) → an `EscapeJump`.
                self.seal(if f.escape {
                    Terminator::EscapeJump(f.exit)
                } else {
                    Terminator::Jump(f.exit)
                });
            }
            StmtKind::Continue => {
                let Some(f) = self.loops.last().copied() else {
                    self.push(HirInstr::Stmt(self.statement(s.clone())));
                    return;
                };
                self.seal(if f.escape {
                    Terminator::EscapeJump(f.header)
                } else {
                    Terminator::Jump(f.header)
                });
            }
            StmtKind::Return(e) => self.seal(Terminator::Return(e.as_ref().map(|e| self.expr(e)))),

            StmtKind::VarDecl { name, ty: _, value } => {
                let value = self.expr(value);
                let v = self.declare_var(name);
                let binding_ty = value.checked.and_then(|id| {
                    self.checked_expressions
                        .get(&id)
                        .and_then(|node| node.binding_ty.clone())
                });
                self.push(HirInstr::Bind {
                    dest: v,
                    expr: value,
                    binding_ty,
                });
            }
            StmtKind::RefDecl { name, value } => {
                let value = self.expr(value);
                let runtime = self.declare_var(name);
                let mut statement = s.clone();
                statement.kind = StmtKind::RefDecl {
                    name: self.vars[runtime as usize].clone(),
                    value: value.syntax,
                };
                self.push(HirInstr::Stmt(self.statement(statement)));
            }
            StmtKind::Assign { name, value } => {
                let value = self.expr(value);
                let captured = self.captures.remove(name);
                if !self.scopes.iter().any(|s| s.contains_key(name))
                    && let Some(scope) = self.scopes.last_mut()
                {
                    scope.insert(name.clone(), name.clone());
                }
                let v = if captured {
                    let runtime = format!("{name}$shadow{}", self.vars.len());
                    if let Some(scope) = self.scopes.last_mut() {
                        scope.insert(name.clone(), runtime.clone());
                    }
                    self.var(&runtime)
                } else {
                    self.var(&self.resolved(name))
                };
                self.push(HirInstr::Bind {
                    dest: v,
                    expr: value,
                    binding_ty: None,
                });
            }
            StmtKind::Expr(e) => self.push(HirInstr::Eval(self.expr(e))),

            // A `try`: snapshot the enclosing loop stack so a `break`/`continue`
            // inside it can escape to an outer loop. Supported only when every
            // enclosing loop is function-level (escapable to the function driver): a
            // function's own loops always are; a region's own loops are region-local
            // and can't be an escape target, so such a `try` falls back to the opaque
            // `Stmt` path (Stage 5 then refuses a crossing `break`/`continue`).
            StmtKind::Try { .. } => {
                let escapable = self.is_function || self.loops.iter().all(|f| f.escape);
                if escapable {
                    let loop_targets = self.loops.iter().map(|f| (f.header, f.exit)).collect();
                    self.push(HirInstr::Try {
                        stmt: self.statement(s.clone()),
                        loop_targets,
                    });
                } else {
                    self.push(HirInstr::Stmt(self.statement(s.clone())));
                }
            }

            // Everything else (member-write, aug-assign, declarations, imports, …)
            // is kept whole for Stage 4; Stage 5 refines it.
            _ => self.push(HirInstr::Stmt(self.statement(s.clone()))),
        }
    }
}
