# Comptime Integration Roadmap

This roadmap focuses on comptime as the next integration axis for mojito. The
goal is not just "more constant folding." The goal is to make compile-time values,
type-level values, value parameters, trait associated compile-time members, and
generic specialization part of one coherent phase boundary.

The practical motivation is clear from the trait roadmap: traits like `Iterable`
probably need an associated element type. mojito already parses trait
`comptime` members:

```mojo
trait Iterable:
    comptime Element: AnyType
```

but the checker currently rejects trait comptime members as unsupported. Folding
that feature into comptime integration is reasonable, and it is probably the
right way to avoid inventing a separate associated-type mechanism later.

This document is broken into phases that should each leave the compiler in a
coherent state. If development needs to switch back to trait work, finish the
current phase, add tests, and stop there.

## Current State

The current comptime implementation has two layers.

The first layer is `src/comptime.rs`, an AST elaboration pass that runs after
module linking and before checking:

```rust
comptime::elaborate(program: Vec<Stmt>) -> Result<Vec<Stmt>, ComptimeError>
```

It supports:

- `comptime NAME = expr`
- `comptime if`
- `comptime for`
- compile-time values: `Int`, `Bool`, `String`, `Tuple`, `List`, and
  compile-time-only `Type`
- VM-backed CTFE calls to pure, top-level functions, including value-parameter
  helpers and helpers whose type parameters are used for compile-time facts
- CTFE support for loops, branches, recursion, and transitive helper calls under
  the shared fuel budget
- pre-VM folding of compile-time-only facts such as `T.size` and
  `is_same_type[T, U]()` into runtime literals before helper lowering
- materialization of module-level compile-time constants into runtime literals

The second layer is in `src/checker.rs`, where value parameters still use a
narrow integer evaluator on top of the shared compile-time value representation:

```rust
fn eval_ct(&self, expr: &Expr) -> Result<i64, TypeError>
```

Checker-side compile-time facts include:

- value parameters represented by `ParamDecl::Value`
- value arguments represented by `TyArg::Val(CtValue)`
- value parameter values restricted to `Int`
- struct substitutions for type parameters, but only symbolic handling for value
  parameters
- trait `comptime` members checked as associated facts; conforming structs can
  satisfy them with explicit `comptime NAME = expr` declarations

`Ty`, `TyArg`, and `ParamDecl` now live in `src/types.rs`, so type-valued
comptime facts can use the same semantic type representation as the checker.
That split is still not fully unified, but the representation foundation is now
shared.

## Design Principles

Keep these invariants in mind through every phase.

1. Comptime is a phase distinction.

   Runtime MIR and the VM should not execute `comptime if` or `comptime for`.
   They should see the selected/generated runtime program.

2. Compile-time values are not runtime locals.

   They do not have runtime addresses, do not get runtime destructors, and do not
   participate in runtime borrowing. They can be materialized into runtime code
   only when they have a runtime representation.

3. Associated type/value members are compile-time facts.

   A trait member like `comptime Element: AnyType` should be represented in the
   same world as value parameters and compile-time constants.

4. Use self-hosted code as the acceptance test.

   Avoid abstract infrastructure without a caller. `Iterable.Element`, generic
   algorithms, and future hash-backed containers are better guides than broad
   theoretical coverage.

5. Fuel remains mandatory.

   Any compile-time execution path must be bounded. Today fuel is fixed and
   program-wide; that can evolve, but compile-time execution must never be
   unbounded.

## Phase 1: Unify The Compile-Time Value Model

Status: complete.

Completion note: the compiler now uses shared `CtValue` as the compile-time
value representation across comptime elaboration and checker value-parameter
handling. `TyArg::Val` carries `CtValue`, symbolic value parameters use
`CtValue::Param`, and the former checker-only value enum is gone. Later phases
have already extended this foundation with type-valued facts.

### Goal

Make `CtValue` the common representation for compiler compile-time values, rather
than having `src/comptime.rs` use `CtValue` and `src/checker.rs` use `i64` /
the former checker-only `CtVal` separately.

This does not require adding type values yet. It is a consolidation phase.

### Current Code To Touch

- `src/comptime.rs`
  - `CtValue`
  - `lit`
  - `materialize_block`
- `src/checker.rs`
  - former `CtVal`
  - `eval_ct`
  - `ParamDecl::Value`
  - `TyArg::Val`
  - `resolve_param_arg`
  - `resolve_use_params`
  - value-parameter checks for `SIMD`, structs, and functions

### Implementation Shape

Move the compile-time value representation into a shared module, for example:

```rust
// src/ct.rs
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CtValue {
    Int(i64),
    Bool(bool),
    Str(String),
    Tuple(Vec<CtValue>),
    List(Vec<CtValue>),
    Param(String),
}
```

`Param(String)` is useful for symbolic value parameters inside a generic body,
replacing the checker-only `CtVal::Param`.

Then:

- replace checker `CtVal` with shared `CtValue`
- replace `TyArg::Val(CtVal)` with `TyArg::Val(CtValue)`
- rename checker `eval_ct()` to something like `eval_ct_int()` only if it still
  returns `i64`
- or better, make it return `CtValue` and use `expect_ct_int()`

Suggested helpers:

```rust
fn expect_ct_int(value: &CtValue, context: &str) -> Result<i64, TypeError>
fn materialize_ct_value(value: &CtValue, span: Span) -> Option<Expr>
fn substitute_ct_value(value: &CtValue, subst: &HashMap<String, CtValue>) -> CtValue
```

### Acceptance Tests

Keep existing tests green first:

- value-parameter structs
- value-parameter functions
- `SIMD[DType.float64, N]`
- module-level `comptime N = ...`
- CTFE-computed value parameter arguments

Add one regression test:

```mojo
def scale[n: Int](x: Int) -> Int:
    return x * n

def pow2(k: Int) -> Int:
    var x: Int = 1
    for i in range(k):
        x = x * 2
    return x

comptime N = pow2(3)

def main():
    print(scale[N](5))
```

### Stop Point

Stop after all existing tests pass and value parameters use the shared compile-
time value representation internally. No type values yet.

## Phase 2: Add Compile-Time Type Values

Status: implemented as representation.

### Goal

Represent types as compile-time values, so associated type members and type-level
generic logic have a natural home.

### Why This Matters

`Iterable` needs a way to say "this iterable's element type is `T`." In a
language like mojito, that is a compile-time fact, not a runtime field.

The parser already supports:

```mojo
trait Iterable:
    comptime Element: AnyType
```

To make that useful, `Element` needs to be able to hold a type.

### Implementation Shape

Extend the shared compile-time value enum:

```rust
pub enum CtValue {
    Int(i64),
    Bool(bool),
    Str(String),
    Tuple(Vec<CtValue>),
    List(Vec<CtValue>),
    Type(Box<Ty>),
    Param(String),
}
```

This phase chose Option A: `Ty` moved to `src/types.rs` and is re-exported from
the crate root with `TyArg` and `ParamDecl`.

The implemented module split is:

```text
src/types.rs
src/ct.rs
src/checker.rs
```

`CtValue::Type` is boxed because `Ty` contains `TyArg`, and `TyArg::Val` contains
`CtValue`; the box breaks that recursive value shape.

Type values are deliberately compile-time-only. They display as their semantic
type and do not materialize into runtime expressions.

### Parser And Expression Support

Decide how users write type values in expressions.

Minimal first form:

- trait declarations can write `comptime Element: AnyType`
- conforming structs can satisfy it implicitly from a type parameter or explicit
  associated member syntax later
- no general runtime-looking expression such as `comptime T = Int` yet

Broader form:

```mojo
comptime T = Int
comptime PairType = Pair[Int]
```

This requires the expression grammar/checker to reinterpret identifiers and type
applications as type values in comptime contexts.

Recommendation: start with trait associated members and internal type values,
then add surface expression syntax once there is a test that needs it.

### Acceptance Tests

Start with checker-level tests rather than runtime tests:

```mojo
trait HasElement:
    comptime Element: AnyType
```

Initially, this should parse and be represented, not rejected as unsupported.

Then add conformance tests in Phase 3.

### Stop Point

Stop when the compiler has a shared representation capable of carrying type
compile-time values, even if user syntax for assigning them is narrow.

## Phase 3: Implement Trait Comptime Members As Associated Facts

Status: implemented with Option A.

### Goal

Make trait `comptime` members real requirements, similar to method requirements.

Example:

```mojo
trait Iterable:
    comptime Element: AnyType
```

A conforming struct must provide an associated compile-time fact named
`Element`, and bounded code should be able to refer to it.

### Current Code To Touch

- `src/ast.rs`
  - `TraitComptime`
- `src/parser.rs`
  - `parse_trait_comptime`
- `src/checker.rs`
  - `TraitInfo`
  - `check_trait`
  - `verify_conformance`
  - `StructInfo`
  - type/member lookup on `Self`
  - type/member lookup on `Ty::Param`

### Data Model

Extend trait metadata:

```rust
struct TraitInfo {
    methods: HashMap<String, MethodSig>,
    comptime_members: HashMap<String, CtMemberReq>,
}

enum CtMemberReq {
    Value(Ty),
    Type { bounds: Vec<String> },
}
```

Extend struct metadata:

```rust
struct StructInfo {
    // existing fields...
    associated: HashMap<String, CtValue>,
}
```

### How Structs Satisfy Associated Members

You need a surface syntax. There are two plausible first increments.

Option A: explicit member in struct body.

```mojo
struct MyList[T: Copyable & Movable](Iterable):
    comptime Element = Self.T
```

This required parser/AST changes because struct bodies previously only allowed
fields and methods.

Option B: derive from conventional names for known builtins/self-hosted types.

For example, for `List[T]`, internally define `Element = T`.

Pros:

- Smaller.
- Good enough for `List`, `Set`, `Dict`, and generic algorithms.

Cons:

- Hardcoded.
- Not a general language feature.

Recommendation: implement Option A. This has been done; it is more
language-shaped and will serve self-hosting better.

### Parser Changes For Struct Associated Members

Extend `StmtKind::Struct` to carry something like:

```rust
associated: Vec<StructComptime>
```

Add:

```rust
pub struct StructComptime {
    pub name: String,
    pub value: Expr,
}
```

Parse in struct bodies:

```mojo
comptime Element = Self.T
```

Do not confuse this with module-level:

```mojo
comptime N = 4
```

Struct-level associated values are declarations on the type, not runtime
statements and not module constants.

### Checker Semantics

When checking a struct:

1. Classify type/value parameters as today.
2. Check associated comptime members in a compile-time environment where:
   - `Self.T` resolves to a type parameter value
   - `Self.n` resolves to a value parameter value
   - simple compile-time literals are allowed
3. Store them in `StructInfo.associated`.
4. When verifying conformance, each trait associated member must exist and have a
   compatible compile-time kind/type.

For a generic struct, associated values may be symbolic:

```rust
Element = CtValue::Type(Box::new(Ty::Param { name: "T", bounds: ... }))
```

When instantiating `List[Int]`, substitute:

```text
Element = Type(Int)
```

### Type Lookup Syntax

You need a way to refer to associated members.

Potential syntax:

```mojo
Self.Element
T.Element
```

`Self.T` already has meaning inside generic structs for type parameters. That
means associated member lookup and type-parameter lookup share syntax. The
checker can resolve in this order:

1. struct type parameter named `T`
2. struct value parameter named `n`
3. associated comptime member named `Element`

For bounded type parameters:

```mojo
def first[C: Iterable](c: C) -> C.Element:
    ...
```

The annotation parser already has member-like `Type` forms? If it does not, add
or reuse existing `Self.X` handling.

### Acceptance Tests

Positive:

```mojo
trait HasElement:
    comptime Element: AnyType

struct Box[T: Copyable & Movable](HasElement):
    comptime Element = Self.T
    var value: Self.T
    def __init__(out self, value: Self.T):
        self.value = value

def main():
    var b: Box[Int] = Box[Int](3)
    print(b.value)
```

Negative:

```mojo
trait HasElement:
    comptime Element: AnyType

struct Bad(HasElement):
    var x: Int
```

Expected: missing associated comptime member `Element`.

### Stop Point

Stop when traits can require associated compile-time members and structs can
satisfy them explicitly. This is now implemented. Do not require generic
algorithms over associated members yet.

## Phase 4: Use Associated Members In Type Checking

Status: implemented.

### Goal

Allow type annotations and generic code to refer to associated type/value
members.

This is where `Iterable.Element` starts becoming useful.

### Example Target

```mojo
trait Iterable:
    comptime Element: AnyType

def first[C: Iterable](c: C) -> C.Element:
    for x in c:
        return x
    raise "empty"
```

This requires two capabilities:

1. `C.Element` resolves to a type in annotations.
2. `for x in c` knows the loop variable has type `C.Element`.

### Type Resolver Changes

Today `ty_from_anno` resolves:

- named builtins
- structs
- type parameters
- `Self`
- `Self.T` style parameter reads

Add associated lookup:

- if base is `Self`, look in current struct associated members
- if base is a type parameter `C`, look through its trait bounds for a member
  requirement named `Element`
- if base is a concrete struct type, look in `StructInfo.associated`

For bounded `C: Iterable`, `C.Element` may remain symbolic:

```rust
Ty::Assoc {
    base: Box<Ty>,
    name: String,
}
```

or it may resolve to the requirement's declared kind and later substitute when
the generic function is instantiated.

Recommendation: add an explicit `Ty::Assoc` if direct substitution becomes too
awkward. It makes diagnostics and deferred generic checking clearer.

### For-Loop Element Typing

Currently for-loop typing knows:

- `range` -> `Int`
- `List[T]` -> `T`
- user struct -> validate `__iter__`

For an opaque bounded type:

```mojo
def f[C: Iterable](c: C):
    for x in c:
        ...
```

the checker needs to accept `C` as iterable if `C` has an `Iterable` bound and
produce loop variable type `C.Element`.

Add:

```rust
fn iterable_element_ty(&self, ty: &Ty) -> Result<Ty, TypeError>
```

Cases:

- `Ty::Range` -> `Int`
- `Ty::List(elem)` -> `elem`
- `Ty::Struct(..)` -> current user iterator protocol
- `Ty::Param { bounds, .. }` with `Iterable` -> associated type `Element`

### Acceptance Tests

Positive:

```mojo
trait Iterable:
    comptime Element: AnyType

def count_items[C: Iterable](c: C) -> Int:
    var n: Int = 0
    for item in c:
        n = n + 1
    return n
```

Then test it with a concrete self-hosted collection once conformance exists.

Negative:

```mojo
def bad[T: AnyType](x: T):
    for item in x:
        pass
```

Expected: not iterable.

### Stop Point

Stop when associated members can be used in type annotations and for-loop typing
for opaque bounded type parameters. This is now implemented; associated lookups
remain symbolic for opaque parameters and resolve to concrete types after generic
substitution when the base type is known.

## Phase 5: Generic CTFE And Specialization

Status: implemented with Approach A.

### Goal

Allow compile-time execution to work with generic functions and type/value
parameters in a controlled way.

The first CTFE increment only called pure, non-generic, top-level functions. The
self-hosting pressure quickly wants:

```mojo
def width[T: SomeTrait]() -> Int:
    return T.some_comptime_fact

comptime W = width[Int]()
```

### Design Choices

There are two approaches.

Approach A: specialize then interpret AST.

- Resolve type/value arguments.
- Substitute them into a CTFE function body.
- Run the existing AST interpreter.

Pros:

- Smaller step from current CTFE.

Cons:

- Grows the second interpreter.
- May diverge from runtime semantics.

Approach B: lower restricted CTFE functions to MIR/VM.

- Run checker/lowering for a pure restricted function.
- Execute it in a compile-time VM environment.
- Convert return `Value` to `CtValue`.

Pros:

- Less duplicated semantics long-term.
- Matches the VM direction.

Cons:

- Needs a compile-time heap/value boundary.
- Harder to prevent runtime effects.

Recommendation: do Approach A only for very small generic compile-time helpers,
then plan a MIR/VM-backed CTFE phase before the AST interpreter grows too large.

### Purity And Effect Rules

CTFE functions should reject:

- `print`
- `raise`
- runtime heap mutation, unless the heap is explicitly compile-time
- pointer allocation/free
- calls to non-CTFE functions
- non-deterministic builtins

They may allow:

- local variables
- arithmetic
- tuples/lists of `CtValue`
- branches/loops with fuel
- calls to other CTFE-safe functions
- reading associated comptime members

### Acceptance Tests

Positive:

```mojo
def choose_width[n: Int]() -> Int:
    if n < 8:
        return 8
    return n

comptime W = choose_width[4]()
```

Positive with associated value:

```mojo
trait Fixed:
    comptime size: Int

struct Buffer[n: Int](Fixed):
    comptime size = Self.n
    var tag: Int

def capacity[T: Fixed]() -> Int:
    return T.size
```

Negative:

```mojo
def bad() -> Int:
    print("no")
    return 1

comptime X = bad()
```

### Stop Point

Stop when generic CTFE helpers can read type/value parameters and associated
comptime members under fuel, without supporting arbitrary runtime behavior. This
is now implemented by specializing CTFE calls into a compile-time environment
and interpreting the existing AST under the same fuel quota.

## Phase 6: Delayed Generic Body Checking For `comptime if`

Status: implemented

### Goal

Make `comptime if` inside generic code select the valid branch after type/value
parameters are known.

### Why This Matters

The major metaprogramming win is code like:

```mojo
def f[T: AnyType](x: T):
    comptime if T is Int:
        print(x + 1)
    else:
        print(x)
```

If both branches must type-check before `T` is known, this pattern fails.

### Current Behavior

The elaborator runs before checking, over the whole AST. That works for module
constants and non-generic code, but generic bodies may need delayed elaboration
because their compile-time conditions depend on type/value parameters.

### Implementation Shape

Add a second elaboration mode:

1. Early elaboration:
   - resolves module-level comptime constants
   - resolves non-generic `comptime if/for`
   - leaves generic-dependent comptime constructs in place or marks them delayed

2. Specialization-time elaboration:
   - runs after generic type/value arguments are known
   - evaluates `comptime if/for` inside the specialized generic body
   - checks only the selected/generated body

This likely requires preserving generic function/struct bodies in a form that can
be elaborated per instantiation, not just checked once with opaque parameters.

### Needed Representation

Introduce a notion of dependency:

```rust
enum CtDependency {
    Independent,
    DependsOnTypeParam(String),
    DependsOnValueParam(String),
}
```

or simply have comptime evaluation return:

```rust
enum CtEval<T> {
    Known(T),
    Dependent,
    Error(...),
}
```

A dependent comptime expression inside a generic should not be an immediate
error. It should be delayed.

### Acceptance Tests

Positive:

```mojo
def f[n: Int]() -> Int:
    comptime if n == 0:
        return 10
    else:
        return 20

def main():
    print(f[0](), f[1]())
```

Negative:

```mojo
def f[n: Int]() -> Int:
    comptime if n == 0:
        return 1
    else:
        return "bad"

def main():
    print(f[0]())
```

Only `f[0]` should be accepted; `f[1]` should reject if instantiated.

### Stop Point

Stop when generic value-parameter `comptime if` works. Type-parameter predicates
can wait if necessary.

## Phase 7: Type Predicates And Type Pattern Matching

Status: implemented

### Goal

Allow compile-time code to ask questions about types.

Examples:

```mojo
comptime if T is Int:
    ...

comptime if T conforms_to Comparable:
    ...
```

### Required Pieces

- `CtValue::Type`
- type equality
- type display in diagnostics
- a syntax for type predicates
- checker/elaborator support for dependent type expressions

Possible syntax choices:

- `T == Int` in comptime contexts
- `T is Int`
- builtin functions like `is_same_type[T, U]()`
- builtin functions like `conforms_to[T, Comparable]()`

Recommendation: start with builtin CTFE predicates rather than new syntax.

Example:

```mojo
comptime if is_same_type[T, Int]():
    ...
```

This avoids touching the parser until the model is proven.

### Acceptance Tests

Positive:

```mojo
def name[T: AnyType]() -> String:
    comptime if is_same_type[T, Int]():
        return "int"
    else:
        return "other"
```

Negative:

Use a type predicate in a runtime `if` and reject it if it cannot be materialized
as a runtime `Bool`.

### Stop Point

Stop when type equality predicates work in generic `comptime if`.

## Phase 8: MIR/VM-Backed CTFE

Status: implemented as VM-backed helper execution with compile-time fact folding.

### Goal

Retire the growing AST CTFE island before it becomes a second runtime.

### Motivation

The tree-walker was retired because language semantics belong in the compiler and
VM path. CTFE should not accidentally recreate the same long-term maintenance
problem.

### Implemented Architecture

Add a restricted compile-time execution backend:

```text
AST generic/specialized function
  -> checker with CTFE restrictions
  -> HIR/MIR
  -> VM in compile-time mode
  -> CtValue
```

Compile-time VM mode should:

- use the same expression/operator semantics as runtime VM
- have a separate compile-time heap
- prohibit I/O and other runtime effects
- enforce fuel
- return values convertible to `CtValue`

The implemented path exposes a narrow VM API for "run this named top-level
function and return its value." The comptime elaborator uses it when a transitive
safety walk proves the helper call graph is CTFE-safe:

- value-parameter generics are allowed and their value arguments are reified into
  VM frame locals
- loops, branches, recursion, and calls to other proven-safe helpers are allowed
- a small deterministic builtin set is allowed (`range`, numeric conversions,
  `abs`, `min`, `max`, `round`)
- VM execution burns the same program-wide comptime fuel as the elaborator
- `Value` results convert back to `CtValue` only for runtime-materializable
  compile-time values (`Int`, `Bool`, `String`, `Tuple`, `List`)

The VM path still deliberately rejects runtime effects and runtime-only machinery:
`print`, `raise`, methods/dunder dispatch through user values, pointer allocation,
`try`, nested declarations, keyword calls, and other unsupported runtime forms.

Type-valued and associated-member facts do not cross into the VM as runtime
values. Instead, the elaborator resolves them before lowering and rewrites the
accepted helper body to contain ordinary runtime literals. For example, `T.size`
inside a CTFE helper can become `8`, and `is_same_type[T, Int]()` can become
`True`, before HIR/MIR lowering begins.

The old AST function-body CTFE interpreter is retired. The remaining evaluator in
`src/comptime.rs` is an expression-level elaboration tool: it chooses
`comptime if` branches, unrolls `comptime for`, resolves type-valued facts, and
performs the pre-VM folding needed by CTFE helpers.

### Fuel Accounting

Fuel should become explicit in the CTFE execution context:

```rust
struct CtContext {
    fuel: usize,
    stack: Vec<CtFrameInfo>,
}
```

Burn fuel for:

- function calls
- loop iterations
- basic block transitions or statement execution
- allocations, if compile-time allocation is allowed

Diagnostics should include the compile-time call stack when fuel is exhausted.

### Acceptance Tests

Port existing CTFE tests to the MIR/VM path without changing behavior:

- recursion
- loops
- arithmetic
- string concatenation
- fuel exhaustion

Add tests that exercise CTFE through constructs the old AST CTFE path did not
own, while staying inside the restricted VM-safe subset.

### Stop Point

Stop when the existing runtime-value CTFE suite runs through the MIR/VM-backed
engine and direct type-valued/associated-member reads are folded before VM
lowering. This is implemented. A future phase should generalize this into true
CTFE helper specialization so nested generic helpers can be cloned per
compile-time argument tuple instead of relying on facts visible at the CTFE entry
helper.

## Phase 9: Generated Declarations

### Goal

Allow comptime to generate declarations, not just statements inside already
parsed bodies.

### Why Later

This is powerful but easy to overbuild. Current `comptime for` already splices
statements. Declaration generation should wait until associated members,
specialization, and CTFE values are settled.

### Possible Forms

```mojo
comptime for name in fields:
    def generated_...:
        ...
```

or generated members inside a struct:

```mojo
struct S:
    comptime for f in fields:
        ...
```

### Required Work

- hygienic name generation
- declaration insertion points
- duplicate declaration diagnostics
- spans for generated declarations
- interaction with module linking
- interaction with generic specialization

### Stop Point

Stop after a single narrow declaration-generation feature, not a general macro
system.

## Cross-Cutting Work

### Diagnostics

Comptime errors need source context and phase context:

- where the comptime expression appears
- which generic instantiation triggered it
- CTFE call stack
- remaining/used fuel
- whether an expression was runtime-only or dependent

### Spans

Generated statements should preserve a useful source span. For unrolled
`comptime for`, diagnostics should point to the source body and ideally mention
the iteration value.

### Caching

Generic specialization and CTFE will need memoization:

```text
function name + type args + value args -> checked/lowered specialization
CTFE function + args -> CtValue
```

Do this only after correctness is clear.

### Soundness Boundaries

Keep compile-time and runtime ownership separate:

- CTFE values are not runtime owners.
- runtime destructors do not run on `CtValue`
- materialized runtime values follow ordinary runtime ownership after insertion

### Documentation

Update these docs as phases land:

- `docs/architecture.md`
- `docs/frontend.md` if syntax changes
- `grammar.md`
- `stdlib/README.md`
- `roadmap.md`

## Recommended Near-Term Sequence

If you want a clean run of comptime work before returning to trait semantics,
take these phases in order:

1. Phase 1: shared `CtValue`
2. Phase 2: type-valued comptime representation
3. Phase 3: trait/struct associated comptime members
4. Phase 4: use associated members in type checking

That sequence directly unlocks the trait roadmap's hardest piece:

```mojo
trait Iterable:
    comptime Element: AnyType
```

After Phase 4, it is reasonable to switch back to trait work and implement
`Sized`, `Iterable`, and `Iterator` with a much better foundation.

If you want to stay in comptime longer, continue with:

5. Phase 5: generic CTFE and specialization
6. Phase 6: delayed generic `comptime if`
7. Phase 7: type predicates

MIR/VM-backed CTFE is now in place, so the next useful comptime work is richer
specialization around it: nested generic helper specialization, generated
declarations, and better CTFE diagnostics.
