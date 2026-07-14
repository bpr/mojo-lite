use super::*;

/// Whether an argument convention transfers ownership to the callee (`owned`, or
/// the destructor's `deinit`).
pub(super) fn is_owned(c: &Option<ArgConvention>) -> bool {
    matches!(c, Some(ArgConvention::Owned | ArgConvention::Deinit))
}

/// Whether a `try` region's statements contain a `break`/`continue` that **leaves**
/// the region — targeting a loop *outside* it. Such an escape would need to name the
/// outer loop's target block, which the self-contained mini-CFG region can't express
/// (unlike a `return`, which surfaces as a `Flow::Return` the block driver handles).
/// Nested loops absorb their own `break`/`continue` (tracked via `loop_depth`);
/// nested `def`/`struct` bodies have their own control flow and are not scanned.
pub(super) fn region_crosses_control(body: &[Stmt]) -> bool {
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
pub(super) fn is_ref(c: &Option<ArgConvention>) -> bool {
    matches!(c, Some(ArgConvention::Mut | ArgConvention::Ref))
}
use crate::hir::VarId;
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
    /// The local reference through which this place is accessed. `None` means
    /// direct owner access. This is static metadata ignored by the VM.
    pub through: Option<VarId>,
}

/// A single three-address instruction. Each value-producing instruction writes a
/// fresh `dest` register; control flow lives in the block's [`MirTerm`].
#[derive(Debug, Clone)]
pub enum MirInstr {
    /// Establish a persistent local loan. The reference has no runtime value in
    /// this lowering; subsequent accesses carry `MirPlace::through` metadata.
    BeginLoan {
        reference: VarId,
        place: MirPlace,
        mutable: bool,
        marker: Reg,
    },
    /// Materialize a runtime reference handle to a verified place. If the root
    /// is already a reference parameter, its handle is forwarded and extended.
    MakeRef {
        dest: Reg,
        place: MirPlace,
    },
    ReadRef {
        dest: Reg,
        reference: Reg,
    },
    WriteRef {
        reference: Reg,
        value: Reg,
    },
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
    /// dataflow *def* (transitions the var to `Owned`). `source_annotation` is a
    /// parsed declaration annotation (`var x: T = …`) so a backend can materialize a numeric literal
    /// to `T` through checked coercion; `None` = inferred `var`/reassignment,
    /// which keeps the binding's existing type (`coerce_like`).
    DefVar {
        var: VarId,
        src: Reg,
        source_annotation: Option<SourceType>,
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
    /// collecting `*args`) via the phase-neutral call matcher.
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
        kwargs: Vec<(String, Reg)>,
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
    /// A structurally lowered `try`/`except`/`else`/`finally` region. Each
    /// sub-part is a self-contained mini-CFG (a
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
    /// caller can coerce a numeric-literal argument at parameter binding. Empty
    /// for `__toplevel__`.
    pub param_annotations: Vec<SourceType>,
    /// Whether each parameter is `owned` (the callee takes ownership, so it drops
    /// the value — unlike a borrowed `read`/`mut` parameter). Same order as the
    /// params; the caller transfers with `^`, so its own drop is skipped.
    pub owned_params: Vec<bool>,
    /// Whether each parameter is a `mut`/`ref` **reference** (its final value is
    /// written back to the caller). `self` (handled via a method's `recv_place`) is
    /// always `false` here. Same order as the params.
    pub ref_params: Vec<bool>,
    pub returns_reference: bool,
    pub spans: SpanTable,
}

/// Maps each generated register to its source span and (if it names one) the
/// origin variable — so borrow-checker diagnostics can point at real code.
#[derive(Debug, Default)]
pub struct SpanTable(pub HashMap<u32 /*reg*/, (SourceSpan, Option<VarId>)>);
