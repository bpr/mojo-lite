//! Phase 1 of the compiler frontend: lower the AST into a **control-flow graph**
//! (CFG) of basic blocks, built on `petgraph`. Expressions stay *nested* here (as
//! embedded AST); Phase 2 flattens them to A-Normal Form. Ownership/liveness
//! analysis (Phase 4) runs over the flattened form, not this one.
//!
//! A [`Cfg`] is built per function body (or the top-level statement list) via
//! [`Cfg::build`]. Every block ends in exactly one [`Terminator`]; control flow
//! (`if`/`while`/`for`/`break`/`continue`/`return`) becomes blocks + edges, so
//! later passes are pure graph traversal.

use crate::ast::{Expr, ExprKind, Stmt, StmtKind, Type};
use crate::token::DUMMY_SPAN;
use petgraph::stable_graph::{NodeIndex, StableGraph};

pub type BlockId = NodeIndex;
pub type VarId = u32;

/// One straight-line instruction inside a basic block. Control flow lives in the
/// block's [`Terminator`], never here.
#[derive(Debug, Clone)]
pub enum HirInstr {
    /// A variable definition (`var x = e`, or a reassignment): a dataflow *def*.
    /// `ty` is the declaration's annotation (`var x: T = …`), carried so a backend
    /// can materialize a numeric literal to `T` exactly as the tree-walker's
    /// `coerce` does; `None` for an inferred `var` or a reassignment (which keeps
    /// the binding's existing type — a `coerce_like`).
    Bind {
        dest: VarId,
        expr: Expr,
        ty: Option<Type>,
    },
    /// A bare expression evaluated for its effect/value (`f(x)`).
    Eval(Expr),
    /// Any other straight-line statement (assignment, member-write, declaration,
    /// …), kept whole for Phase 1 and refined when Phase 2 flattens to MIR.
    Stmt(Stmt),
    /// An ASAP destructor, spliced in by the Phase 4 liveness pass (never by the
    /// Phase 1 lowerer — present so later phases share the type).
    Drop(VarId),
    /// Iterator protocol: normalize the iterable in `iter` to an *iterator* — for a
    /// user struct, `iter = iter.__iter__()`; a built-in `range`/`List` iterates in
    /// place, so this is a no-op. Emitted once before the loop header.
    GetIter { iter: VarId },
    /// Iterator protocol (`for` loops): `dest = whether `iter` yields another
    /// element` (a `Bool`), a pure read of the iterator's state. `iter`/`dest` are
    /// variable slots so both IRs and the backend address them uniformly.
    HasNext { iter: VarId, dest: VarId },
    /// Iterator protocol: `dest = iter.next()` — bind the current element and
    /// advance `iter` in place (a mutating read of the iterator).
    Next { iter: VarId, dest: VarId },
    /// A `try` statement whose sub-regions are lowered (in Phase 2) as mini-CFGs.
    /// `loop_targets` snapshots the enclosing **function-level** loop stack
    /// (`(continue → header, break → exit)`, innermost-last) at this point, so a
    /// `break`/`continue` inside the `try` that targets an outer loop can be
    /// resolved to that loop's block (an `EscapeJump`). Only produced when every
    /// enclosing loop is function-level; otherwise the `try` stays a `Stmt`.
    Try {
        stmt: Stmt,
        loop_targets: Vec<(BlockId, BlockId)>,
    },
}

/// How a block hands control to its successor(s). Exactly one per block.
#[derive(Debug, Clone)]
pub enum Terminator {
    Jump(BlockId),
    Branch {
        cond: Expr,
        then_b: BlockId,
        else_b: BlockId,
    },
    Return(Option<Expr>),
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
    /// assigned. Exposed so the Phase 2 MIR flattener can seed its own interner and
    /// keep `VarId`s consistent across the two IRs.
    pub vars: Vec<String>,
    /// The number of leading `vars` that are the function's **parameters**, in
    /// declaration order (so `vars[0..n_params]` are the params). The call ABI: a
    /// caller binds its argument values to these var slots. `0` for the top-level
    /// block and for bodies built without a parameter list.
    pub n_params: usize,
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
        let mut g = StableGraph::new();
        let entry = g.add_node(BasicBlock::default());
        let mut lower =
            Lower { g, cur: entry, loops: Vec::new(), vars: params.to_vec(), is_function: true };
        for s in body {
            lower.stmt(s);
        }
        lower.seal(Terminator::Return(None)); // implicit `return None` off the end
        Cfg { g: lower.g, entry, vars: lower.vars, n_params: params.len() }
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
        let mut g = StableGraph::new();
        let entry = g.add_node(BasicBlock::default());
        let loops = external_loops
            .iter()
            .map(|&(header, exit)| LoopFrame { header, exit, escape: true })
            .collect();
        let mut lower = Lower { g, cur: entry, loops, vars: seed_vars, is_function: false };
        for s in body {
            lower.stmt(s);
        }
        lower.seal(Terminator::FallOff); // region completed normally (not a return)
        Cfg { g: lower.g, entry, vars: lower.vars, n_params: 0 }
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
    /// Whether this is a function-body CFG (vs. a seeded `try` region). A
    /// function's own loops are function-level, so a `try` inside one can escape to
    /// them; a region's own loops are region-local and cannot be escape targets.
    is_function: bool,
}

impl Lower {
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
                        cond: cond.clone(),
                        then_b,
                        else_b,
                    });
                    self.cur = then_b;
                    self.block(body);
                    self.seal(Terminator::Jump(join));
                    self.cur = else_b; // the next elif/else lowers into this block
                }
                if let Some(body) = orelse {
                    self.block(body);
                }
                self.seal(Terminator::Jump(join));
                self.cur = join;
            }

            StmtKind::While { cond, body } => {
                let header = self.new_block();
                let body_b = self.new_block();
                let exit = self.new_block();
                self.seal(Terminator::Jump(header));
                self.cur = header;
                self.seal(Terminator::Branch {
                    cond: cond.clone(),
                    then_b: body_b,
                    else_b: exit,
                });
                self.cur = body_b;
                self.loops.push(LoopFrame { header, exit, escape: false });
                self.block(body);
                self.loops.pop();
                self.seal(Terminator::Jump(header)); // back-edge (unless body sealed via break/return)
                self.cur = exit;
            }

            // `for x in it: body` — a while-shaped CFG driving the iterator
            // protocol: a pre-header evaluates the iterable once into an iterator
            // slot; the header computes `has_next` and branches on it; the body
            // binds the loop variable via `next` (which advances the iterator).
            StmtKind::For { var, iter, body } => {
                // Evaluate the iterable once into a fresh iterator variable. `$`
                // names can't be produced by the parser, so they never collide with
                // user variables; a monotone `vars.len()` suffix keeps them unique.
                let it_name = format!("$iter{}", self.vars.len());
                let it_var = self.var(&it_name);
                self.push(HirInstr::Bind {
                    dest: it_var,
                    expr: iter.clone(),
                    ty: None,
                });
                // Normalize the iterable to an iterator (a user struct's `__iter__`;
                // a no-op for a built-in `range`/`List`).
                self.push(HirInstr::GetIter { iter: it_var });

                let header = self.new_block();
                let body_b = self.new_block();
                let exit = self.new_block();
                self.seal(Terminator::Jump(header));

                // header: has_next(it) → branch.
                self.cur = header;
                let hn_name = format!("$hasnext{}", self.vars.len());
                let hn_var = self.var(&hn_name);
                self.push(HirInstr::HasNext { iter: it_var, dest: hn_var });
                self.seal(Terminator::Branch {
                    cond: Expr::new(ExprKind::Identifier(hn_name), DUMMY_SPAN),
                    then_b: body_b,
                    else_b: exit,
                });

                // body: x = next(it); <body>; back-edge to header.
                self.cur = body_b;
                let v = self.var(var);
                self.push(HirInstr::Next { iter: it_var, dest: v });
                self.loops.push(LoopFrame { header, exit, escape: false });
                self.block(body);
                self.loops.pop();
                self.seal(Terminator::Jump(header));
                self.cur = exit;
            }

            StmtKind::Break => {
                let f = *self.loops.last().expect("break outside a loop (checker guards this)");
                // A loop in this CFG → a plain `Jump` to its exit; an enclosing
                // function loop (seeded into a `try` region) → an `EscapeJump`.
                self.seal(if f.escape {
                    Terminator::EscapeJump(f.exit)
                } else {
                    Terminator::Jump(f.exit)
                });
            }
            StmtKind::Continue => {
                let f = *self.loops.last().expect("continue outside a loop");
                self.seal(if f.escape {
                    Terminator::EscapeJump(f.header)
                } else {
                    Terminator::Jump(f.header)
                });
            }
            StmtKind::Return(e) => self.seal(Terminator::Return(e.clone())),

            StmtKind::VarDecl { name, ty, value } => {
                let v = self.var(name);
                self.push(HirInstr::Bind {
                    dest: v,
                    expr: value.clone(),
                    ty: ty.clone(), // annotation drives literal coercion
                });
            }
            StmtKind::Assign { name, value } => {
                let v = self.var(name);
                self.push(HirInstr::Bind {
                    dest: v,
                    expr: value.clone(),
                    ty: None, // reassignment keeps the binding's existing type
                });
            }
            StmtKind::Expr(e) => self.push(HirInstr::Eval(e.clone())),

            // A `try`: snapshot the enclosing loop stack so a `break`/`continue`
            // inside it can escape to an outer loop. Supported only when every
            // enclosing loop is function-level (escapable to the function driver): a
            // function's own loops always are; a region's own loops are region-local
            // and can't be an escape target, so such a `try` falls back to the opaque
            // `Stmt` path (Phase 2 then refuses a crossing `break`/`continue`).
            StmtKind::Try { .. } => {
                let escapable = self.is_function || self.loops.iter().all(|f| f.escape);
                if escapable {
                    let loop_targets = self.loops.iter().map(|f| (f.header, f.exit)).collect();
                    self.push(HirInstr::Try { stmt: s.clone(), loop_targets });
                } else {
                    self.push(HirInstr::Stmt(s.clone()));
                }
            }

            // Everything else (member-write, aug-assign, declarations, imports, …)
            // is kept whole for Phase 1; Phase 2 refines it.
            _ => self.push(HirInstr::Stmt(s.clone())),
        }
    }
}
