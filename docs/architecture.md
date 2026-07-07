# Architecture After Parsing

This document describes mojo-lite after parsing. Its input is the parsed program:

```rust
Vec<ast::Stmt>
```

Lexing and parsing are intentionally out of scope here. They will have their own
document. This file starts where the parser stops and follows a program through
semantic checking, HIR lowering, MIR lowering, compiler analyses, drop
elaboration, and execution on the register VM.

## Big Picture

The post-parse pipeline is:

```text
Vec<Stmt>
  -> check
  -> HIR CFG
  -> MIR
  -> ownership / borrow / liveness analysis
  -> drop elaboration
  -> register VM
```

The design is an hourglass:

```text
parsed AST
   |
   v
semantic checker
   |
   v
HIR CFG
   |
   v
MIR  <---- stable waist
   |
   v
analysis + drop elaboration
   |
   v
register VM
```

The MIR is the important waist. Earlier phases preserve source structure and
diagnostics. Later phases should consume verified MIR rather than rediscover
language semantics from the AST.

## Design Goals

The architecture prioritizes:

- correct subset semantics over raw speed
- explicit control flow before ownership analysis
- explicit places before borrowing and partial moves
- deterministic ASAP destruction
- clean rejection of unsupported constructs
- a small compiler that is still recognizable as a systems-language
  implementation

mojo-lite is not trying to be Mojo's production architecture. It has no MLIR
backend, no GPU pipeline, and no optimizer stack. The register VM is the concrete
execution model and the executable specification for the supported subset.

## Stage 1: Semantic Checking

Entry point:

```rust
checker::check(program: &[Stmt]) -> Result<(), TypeError>
```

The checker consumes the parsed AST and rejects programs that the later compiler
does not want to reason about.

It is responsible for:

- names and local scopes
- builtin scalar types
- struct declarations and field layouts
- function and method signatures
- trait declarations and a limited trait-conformance model
- type parameters and value parameters
- call argument matching
- default, keyword, and variadic arguments where supported
- `owned`, `mut`, `ref`, and `deinit` conventions
- simple compile-time integer constants used as value parameters
- list, tuple, string, and SIMD type rules
- borrow checking for call arguments
- rejecting parse-only syntax whose semantics are deferred

The checker is deliberately conservative. If a construct is parsed but not
semantically implemented, this is where it should normally become
`TypeError::Unsupported`.

Examples of syntax that may parse before it is fully implemented include
`comptime if`, `comptime for`, richer trait features, `with`, tuple unpacking,
t-strings, slices, chained comparisons, and ternary expressions.

### Borrow Checking In The Checker

The borrow checker currently lives with call checking because the checked
operation is local to one call expression.

For each argument, the checker classifies the operation:

- ordinary read/shared borrow
- `mut` or `ref` exclusive borrow
- `owned` move via `^`

It then applies the mutable-XOR-shared rule by root/place. The checker is
place-sensitive enough to allow disjoint field borrows such as:

```mojo
f(mut p.a, mut p.b)
```

but reject conflicting uses of the same root/place such as:

```mojo
f(mut p, p)
f(mut p, p^)
```

This early borrow check complements, rather than replaces, MIR ownership
analysis. The checker handles local aliasing at call boundaries; MIR analysis
handles move state across control flow.

### Comptime Today

`comptime` is mostly future architecture.

The checker currently has a narrow compile-time integer evaluator for value
parameters. It can evaluate integer literals, comptime names, unary minus, and
basic integer arithmetic in positions such as SIMD widths or simple
value-parameterized generics.

It does not yet implement real compile-time execution. Planned work includes:

- a real compile-time value model
- `comptime if` branch selection
- `comptime for` unrolling
- CTFE over a restricted VM/MIR subset
- type values
- trait comptime members

See `../comptime.md` for the design notes.

## Stage 2: HIR CFG Lowering

Module:

```rust
src/hir/mod.rs
```

Main type:

```rust
hir::Cfg
```

HIR is the first control-flow-aware representation. It is a graph of basic
blocks backed by `petgraph::StableGraph`. Expressions are still nested AST
expressions at this stage; HIR is about statement control flow, not expression
flattening.

Each HIR block has:

```rust
pub struct BasicBlock {
    pub instrs: Vec<HirInstr>,
    pub term: Option<Terminator>,
}
```

Each block is sealed with exactly one terminator:

```rust
pub enum Terminator {
    Jump(BlockId),
    Branch { cond: Expr, then_b: BlockId, else_b: BlockId },
    Return(Option<Expr>),
    FallOff,
    EscapeJump(BlockId),
}
```

The core invariant is:

> Every block has one terminator, and terminators own the outgoing control-flow
> shape.

That invariant makes later MIR and analysis passes graph-driven rather than
syntax-driven.

### Variable Slots

HIR also interns variables into stable `VarId`s.

```rust
pub type VarId = u32;
```

Function parameters are seeded first, so parameter slots are stable:

```text
vars[0..n_params]
```

This becomes the VM call ABI later. A callee frame receives argument values by
writing them into the first `n_params` variable slots.

### If

An `if`/`elif`/`else` chain lowers to a diamond or chain of diamonds:

```text
current -> branch
          /      \
       then      else/next-elif
          \      /
           join
```

Branches that already returned, broke, or continued are sealed, so the lowerer
does not add spurious join edges.

### While

A `while` lowers to:

```text
preheader -> header -> body -> header
                    \-> exit
```

`break` targets `exit`; `continue` targets `header`.

### For

A `for` lowers to the same control-flow shape as a `while`, with explicit
iterator protocol instructions:

```text
bind iterator
header:
    has_next(iterator) -> Bool
body:
    next(iterator) -> loop variable
    user body
    jump header
exit:
```

This keeps loop control explicit while leaving the runtime details of `range` and
`List` iteration to MIR/VM.

### Try Regions

`try` is represented structurally rather than fully inlining all exceptional
edges into the surrounding CFG.

HIR can emit a special `HirInstr::Try` that carries the original `try` statement
plus a snapshot of enclosing function-level loop targets. This is needed for
source like:

```mojo
for i in range(10):
    try:
        break
    finally:
        print(i)
```

The `break` targets a loop outside the `try` region. A seeded try-region CFG can
therefore produce:

```rust
Terminator::EscapeJump(target)
```

where `target` is a block in the enclosing function CFG, not the local region CFG.
The VM later propagates this as a non-local jump while running `finally` blocks on
the way out.

## Stage 3: MIR Lowering

Module:

```rust
src/mir/mod.rs
```

Main entry points:

```rust
lower_cfg(cfg: &hir::Cfg) -> MirFunction
lower_program(program: &[Stmt]) -> MirProgram
```

MIR is the stable waist of the compiler. HIR still has nested expressions; MIR
flattens them into A-normal form / three-address code.

For example:

```mojo
foo(bar(x + 1))
```

becomes a sequence of register-producing instructions:

```text
r0 = use x
r1 = const 1
r2 = r0 + r1
r3 = call bar(r2)
r4 = call foo(r3)
```

Every intermediate value gets a virtual register:

```rust
pub struct Reg(pub u32);
```

Every variable remains a `VarId` slot:

```rust
pub type VarId = u32;
```

The VM frame has both:

```text
regs: Vec<Value>
vars: Vec<Value>
```

Registers hold temporaries. Variable slots hold source-level locals,
parameters, and synthetic locals such as iterators.

### MIR Program Shape

A lowered program contains:

- one synthetic `__toplevel__` function
- one `MirFunction` per top-level `def`
- one `MirFunction` per lowered struct method
- lifted nested functions where the compiler can safely lift them

The VM runs `__toplevel__`, then calls zero-argument `main()` if it exists.

### MIR Blocks And Terminators

MIR blocks are simple:

```rust
pub struct MirBlock {
    pub instrs: Vec<MirInstr>,
    pub term: MirTerm,
}
```

Terminators are:

```rust
pub enum MirTerm {
    Jump(MirBlockId),
    Branch { cond: Reg, then_b: MirBlockId, else_b: MirBlockId },
    Return(Option<Reg>),
    FallOff,
    EscapeJump { target: MirBlockId, cleanup: Vec<VarId> },
}
```

Function bodies should not normally end with `FallOff`; that is for try
sub-regions. `EscapeJump` is for a `break`/`continue` inside a try region whose
target belongs to the enclosing function.

### Places

MIR separates rvalues from writable places.

```rust
pub struct MirPlace {
    pub root: VarId,
    pub proj: Vec<Proj>,
}

pub enum Proj {
    Field(String),
    Index(Reg),
}
```

A place is something that can be read, written, moved from, or borrowed:

```text
x
p.field
p.items[i].x
xs[i]
```

This is one of the key architecture choices. Mojo-like ownership and borrowing
need to know the difference between "the value computed by an expression" and
"the storage location this expression names." MIR makes that difference explicit.

### Important MIR Instructions

Representative instructions:

```rust
Const
UseVar
MovePlace
DefVar
UnOp
BinOp
Call
MethodCall
GetField
Index
Store
LoadPlace
MakeList
MakeTuple
MakeSimd
Raise
Try
DropVar
HasNext
Next
Unsupported
```

`UseVar` is tagged with a `UseMode`:

```rust
pub enum UseMode {
    Copy,
    Move,
    BorrowShared,
    BorrowMut,
}
```

This lets later analysis distinguish ordinary reads, ownership transfers, and
borrows without reparsing expressions.

### Partial Moves

Whole-variable moves use:

```rust
UseVar { mode: UseMode::Move, ... }
```

Field moves use:

```rust
MovePlace { place, ... }
```

This allows the ownership analysis to understand:

```mojo
var x = p.a^
print(p.b)
```

as valid when `a` and `b` are distinct fields, while rejecting a later read of
`p.a` or a whole-value move of `p` before `p.a` is reinitialized.

### Calls

MIR calls keep the information the VM needs for Mojo-style conventions:

- positional argument registers
- keyword argument registers
- simple caller places for `mut`/`ref` write-back
- compile-time parameter argument registers for value parameters

For method calls, MIR also records whether the receiver was a writable place.
That lets a `mut self` method write the mutated receiver back to the caller.

### Try In MIR

`MirInstr::Try` contains mini-CFGs:

```rust
Try {
    body: Vec<MirBlock>,
    handler: Option<(Option<VarId>, Vec<MirBlock>)>,
    orelse: Option<Vec<MirBlock>>,
    finalbody: Option<Vec<MirBlock>>,
    cleanup: Vec<VarId>,
}
```

Those mini-CFGs share the enclosing function's register and variable spaces. They
have local block numbers, but their instructions address the same `regs` and
`vars` vectors as the outer function frame.

The structure mirrors source-level exception semantics:

- body runs first
- `except` handles a raised error
- `else` runs only when the body completes normally
- `finally` runs on every path
- `return`, `break`, and `continue` crossing the region are represented as
  non-normal flows so `finally` can run before control leaves

### Spans

MIR records source spans for generated registers:

```rust
pub struct SpanTable(pub HashMap<u32, (Span, Option<VarId>)>);
```

This is what lets ownership diagnostics point back to the original source even
though expressions have been flattened into temporaries.

## Stage 4: Ownership Analysis

Module:

```rust
src/analysis/mod.rs
```

Entry point:

```rust
check_ownership(program: &[Stmt]) -> Result<(), OwnershipError>
```

This stage lowers the program to MIR and runs move/init analysis on each
function.

The core state is:

```text
Owned
Moved
MaybeMoved
```

Analysis is forward over the MIR CFG.

Rules:

- defining a variable makes it `Owned`
- moving a variable makes it `Moved`
- using a `Moved` variable is a use-after-move error
- merging `Owned` and `Moved` at a join produces `MaybeMoved`
- using a `MaybeMoved` variable is a conditional-move error
- moving a field marks that field moved but leaves sibling fields usable
- reassigning a moved variable or moved field reinitializes it

This is why control-flow lowering happens before ownership analysis. A move
inside an `if` or loop only has the right meaning once joins and back-edges are
explicit.

### Loops

Loops matter because a move in one iteration can affect the next iteration.

For example:

```mojo
var x = Box(1)
for i in range(3):
    var y = x^
```

The back-edge makes the moved state flow to the next iteration. The analysis can
therefore reject the second iteration's attempted move.

### Partial Move Tree

The analysis tracks places at field granularity. This is stricter and more useful
than only tracking whole variables.

It can distinguish:

```mojo
var a = p.left^
print(p.right)   # ok
print(p.left)    # error
```

Dynamic indexed moves are more conservative because arbitrary indices can alias.

## Stage 5: Liveness And ASAP Destruction

Same module:

```rust
src/analysis/mod.rs
```

Entry point:

```rust
elaborate_drops_program(prog: MirProgram) -> MirProgram
```

ASAP destruction is implemented as a MIR rewrite. The analysis computes where
variables stop being live and splices explicit:

```rust
MirInstr::DropVar { var }
```

after each variable's last use.

The VM does not need to discover last uses dynamically. It just executes
`DropVar` where the compiler placed it.

### What Gets Dropped

Droppable roots are:

- locals
- `owned` parameters

Borrowed parameters are not dropped by the callee. They are owned by the caller.
`self` is handled carefully to avoid destructor recursion and to support method
write-back.

### Drop Order

When several variables die at the same point, they are dropped in reverse
declaration order. Struct destruction runs:

1. the struct's `__del__(deinit self)`, if present
2. fields in reverse declaration order

Lists and tuples drop their elements in reverse order.

### Edge Drops

Some values die on control-flow edges rather than immediately after an
instruction. The liveness pass handles these by inserting drops:

- at the end of the predecessor when there is only one successor
- at the start of the successor when there is only one predecessor
- in a fresh split block for critical edges

This keeps ASAP destruction precise across branches.

### Try Cleanup

Try regions need cleanup for variables defined inside the body. The drop
elaboration pass fills `MirInstr::Try.cleanup` so the VM can destroy body-local
values when the body exits through normal completion, raise, return, break, or
continue.

`EscapeJump` also carries cleanup for cross-region loop escapes. This makes
hidden try-region exits explicit enough for the VM to run destructors before
jumping to the enclosing loop target.

## Stage 6: Register VM

Module:

```rust
src/backend/vm.rs
```

The register VM executes verified MIR. It is structured rather than
byte-addressable:

- registers hold rich `runtime::Value`s
- variables are frame slots
- structs, lists, tuples, strings, errors, ranges, and SIMD values are ordinary
  runtime values
- field and index operations work through high-level value navigation
- calls allocate a new VM frame

The frame shape is:

```text
regs: Vec<Value>
vars: Vec<Value>
```

`regs` are temporaries. `vars` are source variables, parameters, and compiler
synthetic locals.

### Program Metadata

The VM builds a `Prog` containing:

- lowered MIR
- struct definitions and field layouts
- method mutability information
- function signatures
- default arguments
- value-parameter declarations

Some of this metadata is still recovered from the AST because MIR intentionally
does not yet carry every declaration fact. Over time, more of this should migrate
into MIR or a checked declaration table.

### Function Calls

Calling a function:

1. resolves the function index
2. matches arguments to parameters
3. coerces arguments to parameter types
4. creates a new frame
5. writes arguments into parameter variable slots
6. binds value parameters into frame locals
7. runs the callee's block loop
8. returns the result and, where needed, final variable slots for write-back

`mut` and `ref` parameters are implemented by write-back. The caller supplies a
simple place for such arguments. After the callee returns, the VM copies the
callee's final parameter value back into the caller place.

### Method Calls

Method calls are normal function calls with a receiver convention:

- `self` is parameter slot 0
- `mut self` writes the final receiver back to the caller place
- ordinary `mut`/`ref` method parameters also write back
- list mutators operate through the receiver place

### Moves At Runtime

Static ownership analysis should reject invalid moves before execution. The VM
still models move effects:

- moving a variable transfers the value out of the source slot
- the source slot becomes moved/empty
- moving a field leaves a moved marker in that field
- using a moved slot at runtime is a loud error, not silent behavior

This makes the VM a useful backstop and executable model for ownership semantics.

### DropVar At Runtime

`DropVar` removes the value from a variable slot and recursively destroys it.

Dropping is a no-op for scalars and destructor-less values. For structs with
`__del__`, it calls the destructor and then drops fields. Moved-out fields are
skipped so partial moves do not double-drop.

### Exceptions And Non-Normal Flow

`raise` propagates as a runtime `Raised` error until a `Try` catches it.

Inside try sub-regions, the VM uses a control-flow enum conceptually like:

```rust
Normal
Return(Value)
Jump(MirBlockId)
```

This lets `return`, `break`, and `continue` cross a `try` boundary while still
running cleanup and `finally`.

The rule is:

- body raise goes to `except`, if present
- `else` runs only after normal body completion
- `finally` always runs
- non-normal flow from `finally` overrides the pending body/handler/else outcome

## Runtime Values And Builtins

Module:

```rust
src/runtime/mod.rs
```

The VM operates on `runtime::Value`, the shared representation for supported
runtime values:

- integers, unsigned integers, floats, booleans
- strings
- `None`
- structs
- lists
- tuples
- ranges
- SIMD-like lane vectors
- errors
- moved/tombstone markers

Runtime helpers implement:

- arithmetic and comparison
- prefix operators
- coercion and numeric conversion
- string display
- list methods
- SIMD construction and lane access
- builtin functions such as `print`, `len`, `range`, numeric conversions,
  `abs`, `min`, `max`, and `round`

Keeping value-level behavior in `runtime` prevents the VM from baking every
operation directly into the backend. The VM should be a consumer of checked MIR
plus runtime primitives, not a second checker.

## Unsupported Constructs

Unsupported constructs should be explicit.

Preferred behavior:

- parser accepts Mojo-like syntax when possible
- checker rejects unsupported semantics early when it can
- MIR may contain `MirInstr::Unsupported` for late-discovered backend gaps
- VM reports a clean `RuntimeError::Unsupported`
- tests assert unsupported behavior instead of allowing panics

This is important because mojo-lite parses more syntax than it fully implements.
A clean unsupported error is part of the architecture.

## Fixture And Test Relationship

The architecture is reflected in test layout:

- parser tests check AST shape
- checker tests check type and semantic acceptance/rejection
- HIR tests check CFG shape
- MIR tests check lowering shape
- ownership tests check move analysis
- drops tests check ASAP destruction
- VM tests check execution
- `assets/` fixtures exercise whole-pipeline behavior

Accepted `.mojo` programs belong in:

```text
assets/ok/
```

Ownership-specific fixtures belong in:

```text
assets/ownership_ok/
assets/ownership_error/
```

The asset harness turns examples into executable documentation. A feature is more
real when it has a fixture.

## Architectural Boundaries

### Checker vs MIR Analysis

The checker should answer questions that are local to declarations,
expressions, types, and calls.

MIR analysis should answer questions that require control flow:

- has this value been moved on all paths?
- has it been maybe-moved on one path?
- where is the last use?
- where should destruction occur?
- which branch edge needs a cleanup block?

### HIR vs MIR

HIR owns statement-level control flow while expressions remain nested.

MIR owns expression flattening, register allocation, places, and instruction
semantics.

If a feature needs to know the order of subexpression evaluation, it belongs in
MIR or later. If it needs only branch/loop shape, HIR is the right layer.

### MIR vs VM

MIR should preserve enough semantic facts that the VM does not need to infer
language rules from source syntax.

The VM may still hold runtime metadata such as struct field layouts and function
signatures, but the direction should be toward checked declarations and MIR
metadata becoming the source of truth.

### Runtime vs Backend

The runtime module owns value operations. The VM owns execution order, frame
management, calls, jumps, drops, and exception flow.

This separation makes it possible to add another backend later without
reimplementing every scalar/list/string/SIMD rule from scratch.

## Current And Future Pressure Points

The main pressure points are:

- comptime needs a real compile-time value universe and elaboration pass
- more declaration facts should move out of VM-side AST registries
- trait support is intentionally incomplete
- generics and value-parameter specialization need a more central
  representation
- exception modeling is structured, not a fully general unwind-edge MIR
- richer closure support would need a clearer capture representation
- diagnostics should continue moving from "correct" to "pleasant"

The likely next architectural expansion is comptime:

```text
checked AST / early HIR
  -> comptime elaboration
  -> ordinary runtime HIR/MIR
```

The goal should be to avoid rebuilding a second tree-walker. Small constant
evaluation is fine, but general compile-time function execution should eventually
reuse MIR/VM machinery with restrictions and fuel.

## Mental Model

Read the compiler from the middle outward:

1. MIR is the contract.
2. HIR exists to make control flow explicit before MIR.
3. The checker prevents unsupported or ill-typed programs from reaching MIR.
4. Analysis proves ownership and inserts destruction.
5. The VM executes what MIR says.

That is the core architecture of mojo-lite after parsing.
