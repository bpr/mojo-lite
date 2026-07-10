# mojito Roadmap

Status: active planning document.

This roadmap starts from the current milestone: the tree-walker is retired, the
VM path is primary, modules exist, VM-backed CTFE exists, trait associated
`comptime` facts exist, overload resolution is signature-aware, and `stdlib/`
contains self-hosted `Optional`, `List`, `Set`, list-backed `Dict`, experimental
hash-backed `HashSet`, hashing helpers, and math helpers.

The north star remains self-hosting. The best next work is to use self-hosted
library code as the acceptance test, especially code that deliberately uses the
new comptime and trait machinery. That gives the compiler a useful kind of
pressure: real library code should decide whether the next compiler task is
deeper CTFE specialization, better associated members, a stronger trait bound,
or more ordinary runtime support.

## Current Recommendation

Status: recommended.

Pause feature work for a short stabilization pass, then pivot back to
self-hosting.

That is not a retreat from the self-hosting north star. It is the best way to
protect it. The last few cycles landed a lot of foundational language surface:
signature-aware overload resolution, hashable/hash-backed collection proofs,
numeric operation traits, self-hosted math helpers, and lifecycle marker traits
that now affect observable copy/move/drop behavior. The compiler is therefore at
a natural consolidation point.

The immediate cleanup target is small and concrete: `cargo clippy --all-targets
-- -D warnings` currently reports six denied warnings, mostly in `src/checker.rs`
plus one oversized MIR helper. Fix those before adding the next language feature.
After that, clean up the few design debts that are most likely to make the next
self-hosted work harder:

1. Factor complex checker types into named aliases or small structs.
2. Consolidate overload signature/lowered-name logic so checker, MIR, and VM do
   not drift.
3. Keep moving VM metadata toward checked declarations instead of AST-shaped
   side tables.
4. Improve diagnostics for trait and marker-trait conformance failures.
5. Re-run `cargo fmt`, `cargo test`, and `cargo clippy --all-targets -- -D
   warnings` as the new local acceptance gate.

Once that pass is complete, return to the self-hosting loop:

1. Write a self-hosted library skeleton.
2. Run it as an asset/self-host test.
3. Let the compiler failure identify the next missing feature.
4. Implement the smallest compiler change that makes the library code honest.
5. Add one positive and one negative test for the compiler feature.

Deeper nested generic CTFE helper specialization is still a natural task, but it
should not be the next isolated compiler project unless self-hosted code asks for
it. The current CTFE path handles direct helper facts such as `T.size` and
`is_same_type[T, U]()` by folding them before VM execution. The next fully
general step would be to clone and specialize nested CTFE helpers per
compile-time argument tuple. That is useful, but it is better driven by a
library pattern that needs it.

## Current Trait List

Status: current target list; many names are recognized but shallow.

`src/checker.rs` recognizes these built-in trait names as bounds:

```text
AnyType
ImplicitlyDeletable
Movable
Copyable
ImplicitlyCopyable
RegisterPassable
TrivialRegisterPassable
Defaultable
Representable
Writable
Writer
Boolable
Intable
Floatable
Indexer
Equatable
Comparable
Hashable
Hasher
Identifiable
Sized
SizedRaising
Iterable
IterableOwned
Iterator
Absable
Powable
Roundable
Ceilable
Floorable
Truncable
CeilDivable
CeilDivableRaising
DivModable
```

Recognizing a name is not the same as implementing its semantics. Treat this
list as a contract map, not a claim of Mojo equivalence.

## What Is Real Today

Status: implemented, with narrow semantics.

- `Copyable`: affects whether generic values can be copied.
- `Movable`: accepted as a bound and useful documentation; move semantics are
  still mostly structural rather than trait-driven.
- `Equatable`: allows `T == T` / `T != T` for opaque `T` and enables generic
  list/set/dict membership patterns.
- `Comparable`: an ordering contract (Phase 4) — permits `<`/`<=`/`>`/`>=` on a
  bounded opaque `T`, and (as in Mojo) also counts as equality-capable.
- `Hashable`: a real hashing contract (Phase 6) — `x.__hash__() -> UInt` on a
  bounded `K` and on built-in scalars (intrinsic, deterministic). It does *not*
  imply `Equatable`.
- Numeric operation traits: `Absable`, `Roundable`, `Powable`, `Intable`,
  `Floatable`, `Boolable`, `DivModable`, `Ceilable`, `Floorable`, `Truncable`,
  `CeilDivable`, and `CeilDivableRaising` enable the corresponding generic
  builtins, operators, or self-hosted math helpers.
- `Sized`/`SizedRaising`: permit `len(x)` on a bounded opaque `T` (Phase 5).
- Function and method overloading: fixed-arity and conservative same-arity
  type-directed overloads resolve in the checker and lower to stable
  signature-qualified VM names.
- User-defined traits: method requirements work for bounded type parameters.
- Trait associated `comptime` members: checked as associated facts; structs can
  satisfy them with explicit `comptime NAME = expr` declarations.
- Associated facts in generic type checking: enough exists for `Iterable`-style
  element facts and `T.size`-style value facts to be useful in direct contexts.
- VM-backed CTFE: supports pure top-level helper execution under fuel, with
  compile-time-only facts folded before VM lowering.

## Phase 1: Consolidate The Self-Hosted Collection Base

Status: complete.

Purpose: make the current self-hosted library feel deliberate before adding more
abstract trait machinery.

### Tasks

- Status: implemented.
  Keep `stdlib/optional.mojo`, `stdlib/list.mojo`, `stdlib/set.mojo`, and
  `stdlib/dict.mojo` as the base proof.

- Status: implemented.
  Update `stdlib/README.md` to document the public surface of `Optional`,
  `List`, `Set`, and `Dict`.

- Status: implemented.
  Add asset/self-host tests for the self-hosted dictionary. Current coverage
  includes `Dict[String, String]`, overwriting an existing key, copying a
  dictionary while preserving value semantics, iterating entries while reading
  both key and value, and an explicit missing-key raise path.

- Status: implemented.
  Decide and document whether `_DictEntry` is public API or an implementation
  detail before adding key/value/item views.

### Skeleton

Put something like this in `assets/ok/self_hosted_dict_more.mojo` or as a new
self-host test fixture:

```mojo
from std.collections.dict import Dict

def main():
    var d: Dict[String, String] = Dict[String, String]()
    d["name"] = "mojito"
    d["phase"] = "self-host"
    d["phase"] = "stdlib"
    print(d["name"])
    print(d["phase"])

    var count: Int = 0
    for entry in d:
        count = count + 1
        print(entry.key)
        print(entry.value)
    print(count)
```

### Likely Compiler Touchpoints

- `tests/self_host_test.rs`: add the acceptance fixture.
- `src/module.rs`: import resolution if the fixture exposes module edge cases.
- `src/checker.rs`: generic indexing, `String` equality, or iterator element
  typing if the test fails during checking.
- `src/backend/vm.rs` and `src/runtime/mod.rs`: dunder dispatch or runtime value
  behavior if the fixture type-checks but fails at execution.

### Stop Point

Stop when the current self-hosted collections have enough tests that later trait
and comptime changes cannot accidentally break them.

## Phase 2: Comptime-Guided Self-Hosted Algorithms

Status: implemented for direct facts; nested helper specialization remains
blocked until demanded.

Purpose: stress the new metaprogramming in small library code before committing
to a larger compiler feature.

This phase should not start with nested CTFE specialization. Start with direct
use of type predicates, value parameters, associated facts, and VM-backed CTFE.
If that code naturally wants nested CTFE helper specialization, promote that
compiler task.

### Tasks

- Status: implemented.
  Add a tiny `stdlib/algorithms.mojo` with generic helpers that branch on type
  facts.

- Status: implemented.
  Add helpers that use value parameters and CTFE-computed constants.

- Status: implemented.
  Add one helper that reads an associated comptime value or type fact directly
  from a type parameter.

- Status: blocked until demanded.
  Add nested generic CTFE helper specialization only when a useful stdlib helper
  needs `outer[T]()` calling `inner[T]()` where `inner` depends on `T` facts.

### Skeleton

Start with direct facts first:

```mojo
def type_tag[T: AnyType]() -> Int:
    comptime if is_same_type[T, Int]():
        return 1
    elif is_same_type[T, String]():
        return 2
    else:
        return 0

def next_pow2(n: Int) -> Int:
    var x: Int = 1
    while x < n:
        x = x * 2
    return x

comptime DEFAULT_CAP = next_pow2(5)

def default_capacity() -> Int:
    return DEFAULT_CAP
```

Then, when you want to test the nested specialization gap:

```mojo
trait HasStaticSize:
    comptime size: Int

struct Tiny(HasStaticSize):
    comptime size = 4

def inner_size[T: HasStaticSize]() -> Int:
    return T.size

def outer_size[T: HasStaticSize]() -> Int:
    return inner_size[T]() + 1

comptime TINY_PLUS_ONE = outer_size[Tiny]()
```

### Likely Compiler Touchpoints

- `src/comptime.rs`:
  - `ctfe_call`
  - `vm_ctfe_call`
  - `rewrite_vm_ctfe_program`
  - `rewrite_vm_ctfe_expr`
  - CT argument resolution and `CtValue` materialization
- `src/checker.rs`:
  - associated member lookup for bounded type parameters
  - type predicate checking if a predicate leaks into runtime code
- `src/types.rs` and `src/ct.rs`:
  - `Ty` / `TyArg` / `CtValue` representation if new facts need richer shape

### Expected Failure

Direct helpers should work. The nested `outer_size[T]()` skeleton may fail until
CTFE helpers are specialized per compile-time argument tuple. That failure is
useful: it gives a concrete reason to implement deeper nested generic CTFE
helper specialization.

### Stop Point

Stop when a small self-hosted algorithm module uses direct comptime facts and
VM-backed CTFE successfully. If nested specialization is needed, stop after
adding one focused positive test and one negative/unsupported test around that
specific shape.

## Phase 3: Iterable, Iterator, And Associated Element Types

Status: implemented for the first self-hosted library path.

Purpose: make iteration over opaque generic containers usable in self-hosted
algorithms.

`Iterable` is where traits and comptime members meet. A generic algorithm often
needs to know the item type:

```mojo
trait Iterable:
    comptime Element: AnyType
```

### Tasks

- Status: implemented foundation.
  Trait associated `comptime` type members can be declared and checked.

- Status: implemented foundation.
  The checker has associated lookup helpers for bounded type parameters.

- Status: implemented.
  Write a self-hosted generic algorithm that accepts `T: Iterable` and consumes
  `T.Element`.

- Status: implemented.
  Decide whether current `List.__iter__` returning `_ListIter[T]` and
  `Set.__iter__` returning `List[T]` should also declare conformances with
  associated `Element`.

- Status: implemented.
  Tighten checker and VM behavior until the library skeleton works without
  special-case container knowledge.

- Status: implemented.
  Trait method requirements can spell receiver conventions such as `self`,
  `mut self`, `owned self`, and `ref self`; conformance checks compare the
  receiver convention as part of the method contract.

### Skeleton

```mojo
trait IterableOfInt:
    comptime Element: AnyType
    def __iter__(self) -> Self
    def __next__(mut self) -> Self.Element

def sum_items[T: IterableOfInt](items: T) -> Int:
    var total: Int = 0
    for item in items:
        total = total + item
    return total
```

This deliberately started with a small trait rather than trying to model all of
Mojo's iterator protocols at once. The current implementation lives in
`stdlib/iterable.mojo`, and `stdlib/algorithms.mojo` uses it through
`first_or[C: Iterable](items: C, default: C.Element) -> C.Element`.

### Likely Compiler Touchpoints

- `src/checker.rs`:
  - `check_trait`
  - `check_struct`
  - `check_conformance`
  - `infer_for_iter`
  - `lookup_trait_assoc_type`
  - `lookup_trait_method`
- `src/hir/mod.rs` and `src/mir/mod.rs`:
  - only if generic `for` lowering needs more explicit iterator protocol facts
- `src/backend/vm.rs`:
  - only if type checking succeeds but iterator calls do not dispatch correctly

### Stop Point

Completed when `first_or[C: Iterable]` could iterate through opaque bounded
types, with the item type coming from the associated compile-time `Element` fact
rather than hardcoded list/range handling.

## Phase 4: Comparable And Ordered Algorithms

Status: implemented.

Purpose: turn `Comparable` from a recognized name into an ordering contract only
when a library helper needs it.

### Tasks

- Status: implemented.
  Added `has_order_bound(&Ty)` in `src/checker.rs`.

- Status: implemented.
  `infer_infix` now types `<`, `<=`, `>`, and `>=` between equal opaque type
  parameters as `Bool` only when the parameter carries a `Comparable` bound.

- Status: implemented.
  `T: Equatable` still grants `==`/`!=` but ordering on such a `T` is a
  `BadOperator` (`has_order_bound` accepts only `Comparable`).

- Status: implemented.
  `Comparable` implies equality-capable behavior in mojito (as in current
  Mojo): `has_equality_bound` already accepts `Comparable`, so a `T: Comparable`
  parameter type-checks both ordering and `==`/`!=`.

### Tests

- `tests/checker_test.rs`: `comparable_bound_permits_ordering` (positive) and
  `equatable_bound_does_not_permit_ordering` (negative).
- `assets/ok/comparable_ordered_algorithms.mojo`: `min_value`/`clamp` executing
  for concrete comparable types.
- `assets/type_error/equatable_no_ordering.mojo`: `<` under only `Equatable`.

### Skeleton

```mojo
def min_value[T: Comparable](a: T, b: T) -> T:
    if b < a:
        return b
    return a

def clamp[T: Comparable](x: T, lo: T, hi: T) -> T:
    if x < lo:
        return lo
    if x > hi:
        return hi
    return x
```

### Likely Compiler Touchpoints

- `src/checker.rs`:
  - `infer_infix`
  - bound helper functions near equality support
  - builtin trait semantics near `BUILTIN_TRAITS`
- `tests/checker_test.rs` or asset fixtures:
  - positive `T: Comparable`
  - negative `T: Equatable` using `<`

### Stop Point

Stop when ordered generic helpers type-check, execute for concrete comparable
types, and reject the same code under only `Equatable`.

## Phase 5: Sized, Indexer, And Small Generic Views

Status: implemented for `Sized`; `Indexer` and `Dict` views deferred.

Purpose: make container-shaped generic code possible without jumping straight to
hash tables or a full iterator model.

### Tasks

- Status: implemented.
  `T: Sized` permits `len(x)` on an opaque `T` — `infer_len` accepts a
  `Ty::Param` whose bound satisfies the new `has_len_bound` helper, returning
  `Int`. The concrete type's `__len__` runs at runtime after type erasure.

- Status: implemented.
  `SizedRaising` does *not* differ in the current (deferred) effect model, so
  `has_len_bound` accepts it identically to `Sized`. It stays a recognized bound
  that promises a length; a real raising-effect distinction is future work.

- Status: deferred.
  `Indexer` is left recognized-but-shallow until the expected index/result
  associated facts (`Index`/`Element`) are clear — subscripting an opaque `T`
  still needs those before it can be typed.

- Status: deferred.
  `Dict` key/value/item views wait on a richer iterator story.

### Skeleton

```mojo
def is_empty[T: Sized](x: T) -> Bool:
    return len(x) == 0

def has_two_or_more[T: Sized](x: T) -> Bool:
    return len(x) >= 2
```

### Likely Compiler Touchpoints

- `src/checker.rs`:
  - builtin `len` typing
  - bound helper equivalent to `has_len_bound`
  - associated member facts if `Indexer` grows `Index` / `Element`
- `src/backend/vm.rs`:
  - no change expected if `len` already dispatches through `__len__`

### Tests

- `tests/checker_test.rs`: `sized_bound_permits_len` (positive, incl.
  `SizedRaising`) and `any_type_bound_does_not_permit_len` (negative).
- `assets/ok/sized_generic_views.mojo`: `is_empty`/`has_two_or_more` executing
  over a built-in `List` and a user struct that declares `Sized` + `__len__`.
- `assets/type_error/anytype_no_len.mojo`: `len` on a `T: AnyType` parameter.

### Stop Point

Stop when `Sized` enables useful generic container helpers and `AnyType` still
does not.

Reached: `Sized`/`SizedRaising` enable `len` on opaque type parameters, `AnyType`
does not. `Indexer` and `Dict` views remain deferred.

## Phase 5b: Function And Method Overloading

Status: implemented for fixed-arity overload sets and conservative same-arity
type-directed overloads.

Purpose: add Mojo-style overload sets before the stdlib grows enough APIs that
single-name functions become an obvious user-facing limitation.

Overloading is more fundamental than hash-backed collections. Users will notice
the lack of ordinary overloads quickly, especially around constructors,
container helpers, numeric utilities, and protocol-shaped APIs where Mojo code
expects one source name with several valid signatures.

This phase should start with arity-based overloads and only then grow toward
full type-directed ranking. That keeps the first implementation useful without
trying to reproduce all of Mojo's overload resolution in one pass.

### Tasks

- Status: implemented.
  Represent top-level overload sets in checker scopes instead of treating a
  repeated `def` name as redeclaration. Distinct fixed arities now merge into a
  `Ty::Overload` candidate set.

- Status: implemented.
  Represent struct and trait method overload sets, replacing
  `HashMap<String, MethodSig>` with a per-name candidate list.

- Status: implemented.
  Resolve calls during type checking by collecting candidates, filtering by
  arity/keyword shape/type arguments, checking coercions, and requiring exactly
  one best match. Exact argument-type matches beat candidates that need
  coercions; ties, such as an integer literal that could become either `Int` or
  `Float64`, are rejected as ambiguous.

- Status: implemented.
  Preserve the selected overload identity for lowering. Source names can remain
  `f(...)`, but MIR/VM needs a unique lowered name such as `f#0`, `f#1`, or a
  stable signature-based mangling. Current lowering uses signature-mangled names
  such as `f$ov$Int` and a checker-produced span-to-callee table for overloaded
  call sites.

- Status: implemented.
  Add method-overload conformance rules: a conforming struct must provide a
  matching implementation for each trait requirement overload, including
  receiver convention.

- Status: implemented for current symbol imports.
  Extend module import/export handling so importing a name imports the whole
  overload set. The overload set is represented as the imported symbol's type,
  so `from module import name` brings the full set with it.

### Skeleton

```mojo
def pick() -> Int:
    return 0

def pick(x: Int) -> Int:
    return x

struct Buffer:
    var n: Int

    def __init__(out self):
        self.n = 0

    def __init__(out self, n: Int):
        self.n = n
```

### Likely Compiler Touchpoints

- `src/checker.rs`:
  - scope representation
  - top-level `def` registration
  - `StructInfo.methods`
  - `TraitInfo.methods`
  - `infer_call`
  - `infer_method_call`
  - conformance checking
- `src/mir/mod.rs`:
  - unique lowered names for overloads
  - call lowering must use the resolved lowered callee, not just the source name
- `src/backend/vm.rs`:
  - function signature registry and `index_of` lookup must use lowered names
- `src/module.rs`:
  - selective imports must bring all overload candidates for an imported name

### Stop Point

Stop when same-name functions and same-name methods with different arities or
clearly distinct argument types type-check, lower, and execute, while ambiguous
or duplicate-equivalent overloads are rejected cleanly.

Reached: top-level functions, methods, and constructors can overload by fixed
arity and by parameter type when one candidate is uniquely best. MIR lowers
overloaded definitions to signature-qualified names such as `pick$ov$Int` and
`Box.__init__$ov$String`; overloaded call sites use the checker-produced
resolved-callee table, so same-arity type-directed calls preserve the static
choice into VM execution. Ambiguous coercion cases remain rejected.

## Phase 5c: Mojo-Shaped Function Argument Lists

Status: partially implemented.

Purpose: make common Mojo function signatures usable without pushing users back
into Python/Rust-shaped call conventions.

Ordinary free functions now support positional-only `/`, keyword-only bare `*`,
keyword calls, default values, homogeneous `*args`, required keyword-only
arguments, and regular parameters after `*args`. The checker and VM use the same
slot matcher, so a call accepted by the checker binds the same way at runtime.

### Tasks

- Status: implemented.
  Enforce that arguments before `/` cannot be supplied by keyword.

- Status: implemented.
  Enforce that arguments after bare `*` must be supplied by keyword.

- Status: implemented.
  Bind `*args` in source-parameter order in the VM frame, including signatures
  such as `def f(*xs: Int, scale: Int)`.

- Status: deferred.
  Implement `**kwargs` once mojito has a real keyword mapping/value story.

- Status: deferred.
  Extend keyword/default argument binding to ordinary method calls. The current
  method-call path is still mostly positional, aside from constructor/copy
  special cases.

- Status: deferred.
  Extend generic function calls to use the same marker-aware keyword/default
  binding as ordinary functions.

## Phase 5d: Module Search Roots And Mojo-Shaped Stdlib Layout

Status: implemented foundation.

Purpose: let beginning users write imports that look like Mojo instead of
repository-relative paths.

`src/module.rs` now has explicit `LinkOptions` search roots. The default
`link(...)` and `link_source(...)` paths search the importing file's directory
first and then the bundled `stdlib/` root, so imports such as
`from std.collections.dict import Dict` work in asset fixtures, tests, and CLI
file execution.

### Tasks

- Status: implemented.
  Mirror the self-hosted library under `stdlib/std/...`.

- Status: implemented.
  Update self-hosted fixtures and self-host tests to use `std...` imports.

- Status: compatibility kept for now.
  Keep the old flat `stdlib/*.mojo` files so existing `from list import List`
  examples do not break immediately.

- Status: pending.
  Consider a CLI `--module-path` or `--stdlib` option if users need custom roots
  outside the bundled repo/test layout.

## Phase 6: Hashable, Hasher, And Hash-Backed Collections

Status: implemented for `Hashable` + a hash-backed collection proof; trait
default methods and `Hasher` deferred.

Purpose: graduate beyond list-backed `Set`/`Dict` only when hashing semantics
are real enough to be worth testing.

Do not rush this phase. A list-backed dictionary is still valuable because it is
simple, inspectable, and useful as a reference implementation.

### Tasks

- Status: decided (implemented as `UInt`).
  Mojo's own `Hashable` doc comment specifies `__hash__(self) -> UInt` (the free
  `hash()` returns `UInt64`). mojito models `__hash__(self) -> UInt` because
  its native `UInt` is a word-sized unsigned integer with full modular
  arithmetic, whereas its `UInt64` is SIMD-backed and lacks `% // **`. So the
  hash *result* is `UInt` — matching the trait signature and keeping bucketing
  (`h % bucket_count`) expressible.

- Status: decided.
  Hashing is deterministic across runs. Do not introduce a randomized hash seed
  unless Mojo itself does; current evidence says Mojo hashing has no per-process
  RNG salt.

  Implemented: `runtime::builtin_hash` is a no-seed FNV-1a over the value's
  bytes, so equal keys hash identically every run.

- Status: decided (implemented).
  `Hashable` does not imply `Equatable`. `has_equality_bound` no longer accepts
  `Hashable`, so a `K: Hashable` parameter cannot use `==`; the hash-backed
  `HashSet` key bounds `Hashable & Equatable` (hash picks a bucket, equality
  resolves collisions).

- Status: implemented via intrinsic + explicit `__hash__`, not default methods.
  `Hashable` contributes `__hash__(self) -> UInt` to a bounded type parameter
  (`lookup_trait_method` synthesizes it), and the built-in scalar types
  (`Int`/`UInt`/`Bool`/`String`/`Float64`) hash intrinsically in the VM
  (`method_call` intercepts `__hash__` on non-struct values). A user key struct
  writes its own `__hash__`. This deliberately routes around general trait
  default methods — a universal default `__hash__` needs field reflection Mojo
  gets from `@fieldwise`/compiler magic that mojito does not model, and the
  hashing proof did not require it.

- Status: deferred.
  General trait default-method support (the 7-step sketch below) remains future
  work. It is a sizable, orthogonal feature (it interacts with method overload
  sets, conformance, synthetic per-struct lowering, and VM metadata) and Phase 6
  reached its stop point without it. Promote it when a stdlib pattern actually
  needs an inherited default body.

  Original implementation sketch (unchanged, for when it is promoted):

  1. Preserve default method bodies in trait metadata instead of rejecting them as
     unsupported. `ast::TraitMethod.default_body` already carries `Some(body)`;
     `src/checker.rs` should split each trait method into signature plus optional
     default body.
  2. Type-check default bodies in a trait context. The receiver type should be
     `Self`/`Ty::SelfType`, trait associated facts should remain available, and
     receiver conventions (`self`, `mut self`, `owned self`, `ref self`) must be
     checked the same way they are for requirements and struct methods.
  3. Define conformance rules: a struct may satisfy a required method with an
     explicit implementation or by inheriting the trait default. If multiple
     traits provide the same default method and the struct does not override it,
     reject the conformance as ambiguous.
  4. Make bounded type-parameter calls see default methods. A call like
     `key.__hash__()` where `K: Hashable` should type-check even when the
     concrete struct did not write its own `__hash__`.
  5. Decide lowering strategy. The conservative first implementation can lower a
     default method once per conforming struct as a synthetic method such as
     `Type.__hash__$default$Hashable`, with `Self` substituted to the struct type.
     That keeps VM dispatch ordinary: after conformance checking, the struct has
     a callable method entry either from its own method or from a synthesized
     default.
  6. Thread synthesized methods through MIR lowering and VM metadata. The current
     VM still builds struct method mutability and signatures partly from the AST;
     default methods need to appear in that declaration table or in an equivalent
     checked-declaration table so runtime dispatch can find them.
  7. Add fixtures before hashing collections: one struct inherits a default
     method, one struct overrides it, and one conformance is rejected because two
     traits provide an ambiguous default with the same signature.

- Status: implemented.
  `stdlib/hashing.mojo` is the tiny hash helper: `bucket_index[K: Hashable]`.

- Status: implemented.
  `stdlib/hashset.mojo` is an experimental hash-backed `HashSet[T]` (fixed bucket
  array, scanned per-bucket), left alongside the list-backed `Set` reference.
  `stdlib/dict.mojo` is unchanged (no hash-backed `Dict` yet).

- Status: deferred.
  `Hasher` (the incremental-hashing trait) is not modeled; hashing is a single
  `__hash__ -> UInt`, not a `Hasher`-driven stream.

### Skeleton

Implemented shape (note `UInt`, not `UInt64` — see the hash-result-type task):

```mojo
def bucket_index[K: Hashable](key: K, bucket_count: Int) -> Int:
    return Int(key.__hash__() % UInt(bucket_count))
```

### Likely Compiler Touchpoints

- `src/checker.rs`:
  - method requirements on bounded type parameters
  - default trait method signature/body checking
  - conformance using explicit methods or default trait methods
  - `%` and numeric typing for generic algorithms
  - `Hashable` must not imply equality
- `src/mir/mod.rs`:
  - synthetic lowered methods for inherited trait defaults, or a checked
    declaration table that MIR lowering can consume
- `src/backend/vm.rs`:
  - method dispatch through opaque/generic values
  - runtime metadata for synthesized default methods
- `src/runtime/mod.rs`:
  - only if concrete builtin types get intrinsic hash behavior

### Stop Point

Stop when a tiny hash-backed collection proof works for one or two key types.
Only then decide whether to replace or keep the list-backed `Dict`.

Reached: `HashSet[Int]` and `HashSet[String]` run (`tests/self_host_test.rs`
`self_hosted_hash_backed_set`). The list-backed `Set`/`Dict` are kept as the
inspectable reference; `HashSet` is an experimental alternative (fixed bucket
count, no rehash), so replacing `Dict` is *not* yet warranted.

### Tests

- `tests/checker_test.rs`: `hashable_bound_permits_hash`,
  `hashable_bound_does_not_permit_equality`, `any_type_bound_does_not_permit_hash`.
- `tests/self_host_test.rs`: `self_hosted_hash_backed_set` (links
  `hashing.mojo` + `hashset.mojo`).
- `assets/ok/hashable_bucketing.mojo`, `assets/type_error/hashable_no_equality.mojo`.

## Phase 7: Numeric Operation Traits

Status: implemented (all clusters).

Purpose: make numeric generic algorithms type-check without treating all
operators as universally available on opaque type parameters.

Each cluster is gated by the shared `param_has_bound(&Ty, name)` helper in
`src/checker.rs`: a numeric-operation bound enables its builtin/operator on an
opaque `T`, returning the operation's result type, and the concrete numeric
type's implementation runs after type erasure (no VM change needed — the builtins
and `**` already execute for `Int`/`Float64`).

### Tasks

- Status: implemented.
  `Absable` -> `abs(x)` (`infer_abs`; returns the same type `T`).

- Status: implemented.
  `Roundable` -> `round(x)` (`infer_round`; returns `T`, matching Mojo's
  `__round__(self) -> Self` — the roadmap's earlier `-> Int` skeleton was wrong).

- Status: implemented.
  `Powable` -> `x ** y` between two `T` (`infer_infix` `Pow` arm; returns `T`).

- Status: implemented.
  `Int(x)` on `T: Intable`, `Float64(x)` on `T: Floatable`, and `Bool(x)` on
  `T: Boolable` (`infer_conversion`; `Bool(x)` is truthiness — a `Bool` prelude
  constructor added as a conversion builtin, checker + `runtime::builtin_convert`
  + VM `call_named`).

- Status: implemented.
  `Ceilable`/`Floorable`/`Truncable`/`CeilDivable`/`CeilDivableRaising` and
  `DivModable`. `divmod(a, b)` is a **prelude** builtin (like `abs`/`round`) →
  `Tuple[T, T]` (`infer_divmod` + `runtime::builtin_divmod`). The rounding
  functions `ceil`/`floor`/`trunc`/`ceildiv` are **not** prelude in real Mojo
  (they live in `math`), so — honoring the strict-subset rule — they are a
  **self-hosted `stdlib/std/math.mojo` module** (`from std.math import …`), whose generic
  bodies call trait-gated intrinsic dunders (`lookup_trait_method` synthesizes
  `__floor__`/`__ceil__`/`__trunc__ -> Self` and `__ceildiv__(Self) -> Self`;
  the VM intercepts them on built-in numeric values via `builtin_round_dir`/
  `builtin_ceildiv`). The Mojo trait `DivModable` (capital M) was corrected in
  `BUILTIN_TRAITS` (was misspelled `Divmodable`).

### Skeleton

Implemented shape (note `round` returns `T`, not `Int`):

```mojo
def absolute[T: Absable](x: T) -> T:
    return abs(x)

def rounded[T: Roundable](x: T) -> T:
    return round(x)
```

### Likely Compiler Touchpoints

- `src/checker.rs`:
  - builtin call typing for `abs`, `round`, conversions, and future builtins
  - operator checking for `**`, division helpers, and result types
- `src/backend/vm.rs` and `src/runtime/mod.rs`:
  - runtime builtin implementations if a supported type has no execution path

### Tests

- `tests/checker_test.rs`: `numeric_operation_traits_permit_their_operation`
  (positive — every bound incl. `Boolable`/`DivModable`/`Floorable`/…) and
  `any_type_bound_does_not_permit_numeric_operations` (negative, incl. that
  `Absable` grants `abs` but not `**`, and `Floorable` grants `__floor__` but not
  `__ceildiv__`).
- `tests/self_host_test.rs`: `self_hosted_math_rounding_helpers` (links
  `stdlib/math.mojo`, runs `floor`/`ceil`/`trunc`/`ceildiv`).
- `assets/ok/{numeric_operation_traits,boolable_and_divmod,math_module_rounding}.mojo`
  and `assets/type_error/{absable_no_power,boolable_required_for_bool}.mojo`.

### Stop Point

Stop after each tiny trait cluster has one positive and one negative test. Do not
turn this into a broad numeric tower project unless the self-hosted stdlib needs
it.

Reached: every cluster type-checks, executes for concrete numerics, and is
rejected under `AnyType`. `Absable`/`Roundable`/`Powable`/`Intable`/`Floatable`/
`Boolable`/`DivModable` use prelude builtins/operators; `Ceilable`/`Floorable`/
`Truncable`/`CeilDivable`/`CeilDivableRaising` use the self-hosted `math` module.

## Phase 8: Ownership And Marker Traits

Status: implemented for lifecycle markers that affect observable copy/move/drop
behavior; layout/backend markers remain deferred.

Purpose: tie Mojo-shaped lifecycle trait names to actual ownership and value
semantics rather than to an allowlist.

### Tasks

- Status: implemented.
  `Copyable` affects whether generic values can be copied and whether concrete
  types satisfy `T: Copyable`. A struct is `Copyable` if it declares `Copyable`
  or `ImplicitlyCopyable`, or if it defines `__copyinit__`. A fieldwise
  `Copyable` declaration is checked: without an explicit `__copyinit__`, all
  fields must themselves be copyable.

- Status: implemented.
  `ImplicitlyCopyable` is stronger than `Copyable`: it means ordinary implicit
  copy is valid without relying on a custom copy constructor. Scalars and builtin
  value types are implicitly copyable. A struct satisfies it only when it
  declares `ImplicitlyCopyable`, does not define `__copyinit__`, and all fields
  are implicitly copyable. Explicit-copy structs can satisfy `Copyable` without
  satisfying `ImplicitlyCopyable`.

- Status: implemented pragmatic.
  `ImplicitlyDeletable` is tied to the existing ASAP destruction model: supported
  value types are implicitly deletable, and a struct can declare the marker when
  all fields are implicitly deletable. A custom `__del__(deinit self)` is still
  allowed; the marker means the compiler may destroy the value automatically, not
  that destruction is trivial.

- Status: implemented.
  `Movable` corresponds to the current ownership model's move capability. All
  initialized supported values are movable; move validity itself is still checked
  by MIR ownership analysis (`^`, partial moves, conditional moves, etc.).

- Status: deferred.
  Treat `RegisterPassable` and `TrivialRegisterPassable` as layout/backend
  concerns unless a VM or future backend feature needs them.

### Skeleton

```mojo
def duplicate[T: Copyable](x: T) -> T:
    var y: T = x
    return y
```

### Likely Compiler Touchpoints

- `src/checker.rs`:
  - copyability checks for generic values
  - lifecycle method recognition
- `src/analysis/mod.rs`:
  - move state and partial move diagnostics
- `src/mir/mod.rs`:
  - drop elaboration inputs and move/copy operations
- `src/backend/vm.rs`:
  - destructor and move/copy execution behavior

### Stop Point

Stop when marker traits correspond to observable ownership behavior, not just
accepted bounds.

Reached: lifecycle marker bounds are no longer blanket-accepted. A move-only
struct fails `T: Copyable`, an explicit-copy struct fails
`T: ImplicitlyCopyable`, fieldwise `Copyable` conformance rejects move-only
fields, and `ImplicitlyDeletable`/`Movable` are checked through the ownership and
ASAP-destruction model. `RegisterPassable`/`TrivialRegisterPassable` are still
recognized but shallow layout/backend markers.

### Tests

- `assets/ok/ownership_marker_traits.mojo`
- `assets/type_error/copyable_bound_rejects_move_only.mojo`
- `assets/type_error/implicitly_copyable_rejects_explicit_copy.mojo`
- `assets/type_error/copyable_field_rejects_move_only.mojo`

## Phase 9: Representable, Writable, Writer

Status: deferred; verify Mojo shape before implementing.

Purpose: avoid inventing a stale `Stringable`-like protocol.

`Stringable` is not a current target. Keep string conversion pragmatic and driven
by concrete builtin conversions plus `__str__`/representation dunders until the
Mojo-shaped protocols are clearer.

### Tasks

- Status: pending research.
  Verify current Mojo meaning before encoding `Representable`, `Writable`, or
  `Writer` semantics.

- Status: implemented pragmatic.
  Keep `print` working for printable VM values.

- Status: pending.
  Use `__str__` as the small user-defined hook where it already works.

- Status: deferred.
  Add self-hosted writer/formatter types only after a real stdlib use case
  appears.

### Skeleton

```mojo
struct BufferWriter:
    var text: String

    def __init__(out self):
        self.text = ""

    def write(mut self, value: String):
        self.text = self.text + value
```

### Likely Compiler Touchpoints

- `src/checker.rs`:
  - `String(...)`, `print`, and `__str__` typing
  - trait method requirements if writer protocols become traits
- `src/runtime/mod.rs`:
  - string concatenation and display behavior
- `src/backend/vm.rs`:
  - print/string conversion dispatch

### Stop Point

Stop when there is one useful self-hosted formatting or writing abstraction. Do
not implement the whole protocol family speculatively.

## Cross-Cutting Rule: Status Lines

Status: ongoing convention.

Every new phase and task in this roadmap should keep a status line. Use one of:

- `Status: implemented.`
- `Status: partially implemented.`
- `Status: recommended next phase.`
- `Status: pending.`
- `Status: pending design.`
- `Status: deferred.`
- `Status: blocked until demanded.`

The point is to make the roadmap easy to resume after a few compiler cycles. The
status should describe the compiler and stdlib reality, not optimism.

## Near-Term Order

Status: recommended.

1. Do the stabilization checkpoint now.
2. Make clippy clean under `cargo clippy --all-targets -- -D warnings`, or
   document any intentional lint allows with narrow local justification.
3. Consolidate the duplicated overload/lowered-name plumbing before extending
   overload resolution further.
4. Tighten trait/marker-trait diagnostics so self-hosted code reports the
   missing capability instead of a generic bound failure.
5. Status: implemented.
   Improve module search roots and move/mirror the stdlib toward
   `stdlib/std/...`, so self-hosted fixtures can import Mojo-shaped modules
   without repository-relative dot paths.
6. Return to self-hosted libraries after cleanup: hash-backed `Dict`, richer
   collection views, and formatting/writer experiments are better next
   acceptance tests than speculative CTFE work.
7. Promote nested generic CTFE helper specialization only when that self-hosted
   code naturally needs it.
8. Promote general trait default methods when a real library protocol needs an
   inherited default body; `Hashable` currently reached its proof through
   intrinsic plus explicit `__hash__`, not a general default-method system.

This keeps the project aligned with the self-hosting north star while clearing
the compiler debt most likely to slow the next few self-hosted features.
