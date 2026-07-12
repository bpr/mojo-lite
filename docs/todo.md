# Todo

This file contains only unfinished, concrete work. Phase order and completed
milestones belong in `roadmap.md`; architectural invariants belong in
`docs/architecture.md`.

Tasks are grouped in the same approximate dependency order as the roadmap.

## Correctness and Interface Gaps

### CLI module search paths

Expose `LinkOptions.search_roots` through the CLI. Decide between repeatable
`--module-path PATH`, a dedicated `--stdlib PATH`, or both. Cover precedence
between the importing directory, user roots, and the bundled stdlib.

### Method argument binding parity

Route ordinary method calls through the same slot matcher used by free
functions. Support defaults, keyword arguments, positional-only and keyword-only
markers, required keyword-only parameters, and homogeneous `*args`. Preserve
`mut self` and `mut`/`ref` argument write-back behavior.

### Generic call binding parity

Use the ordinary marker-aware matcher for generic free-function calls after
compile-time type/value arguments are resolved. Add parity tests for defaults,
keywords, and variadic arguments.

## Self-Hosted Collections

### Nested self-hosted lists

Make `std.collections.list.List[List[T]]` work as the bucket-array representation
inside `std.collections.hashset`. Remove the remaining reliance on the built-in
`List` behavior from that path and retain collision tests for `Int` and `String`.

### Dictionary views

Design key, value, and item views without exposing `_DictEntry` as public API.
Wait until the `Indexer` and iterator associated-result design can state each
view's element type cleanly.

### Hash-backed dictionary

Add a separate hash-backed dictionary after nested self-hosted lists are stable.
Use the list-backed `Dict` as the behavior oracle for insertion, overwrite,
missing keys, iteration, copying, and value semantics. Keep collision resolution
and resizing explicit in tests.

### Keyword-map value

Determine whether the self-hosted dictionary is a suitable runtime value for
`**kwargs`, including key ownership, argument ordering, duplicate detection, and
write-back restrictions. Implement `**kwargs` only after this representation is
settled.

## Checked Semantic Data

### Checked program representation

Define a checked program/declaration representation that owns resolved symbols,
types, conformances, callable signatures, defaults, and source provenance.
Change the checker to produce it without weakening existing sequential name
binding or overload resolution.

### Typed MIR declarations

Build `MirDeclarations` from checked declarations. Replace remaining AST `Type`
and default `Expr` values with checked types and normalized constant/default
metadata so MIR and backends do not reinterpret source syntax.

### Module identity preservation

Identify the smallest provenance needed after the linker flattens modules. Add
module identity to checked declarations only when a concrete consumer—improved
diagnostics, caching, or interchange—needs it.

## Origins, References, and Lifetimes

Mojo origins are compile-time symbolic descriptions of the storage that governs
a reference and of the mutability available through it. They are used to extend
owner lifetimes, enforce exclusivity, and reject references that outlive their
owners. They have no runtime identity and are erased after lifetime checking.

Mojito now parses the principal surface forms, but origin clauses are discarded
and reference bindings/types are deliberately rejected by the checker. Existing
infrastructure provides only part of the semantic foundation:

- MIR places already identify rooted field/index projections.
- Call checking already rejects overlapping mutable/shared argument places and
  permits statically disjoint fields.
- MIR use modes distinguish copy, move, shared borrow, and mutable borrow.
- Ownership analysis already tracks moves through CFG joins and loops.
- Liveness and drop elaboration already insert ASAP destruction and edge cleanup.
- `mut` and `ref` parameters currently use value cloning plus post-call
  write-back, not persistent reference identity.

The last point is the largest architectural gap. A reference binding or ref
return can remain live beyond one call and must continue to alias its owner's
storage. That cannot be modeled faithfully by copying a value into a callee
frame and writing it back once when the call returns.

### Origin representation

Replace discarded origin syntax with typed source data:

- origin expressions for named origins, `origin_of(place)`, arbitrary
  `origin_of` expressions, `_`, and unions
- origin parameters with immutable, mutable, or parameterized mutability
- the `//` infer-only boundary in generic parameter declarations
- origin-bearing `ref` parameter and return types
- source spans for origin diagnostics

Define a small canonical checked algebra rather than retaining arbitrary AST
expressions indefinitely. At minimum it needs owner declarations, field
derivation, union, inferred/unbound origins, static/untracked origins needed by
supported APIs, and mutability.

### Reference type and binding semantics

Add a checked reference type containing the referent type, origin, and
mutability. Type `ref name = expression` as an alias rather than an owned
binding. Require a place-producing expression for safe tracked references, while
keeping any future unsafe/untracked form explicit.

Define how reference reads, writes, assignment, copying of reference handles,
and end-of-reference lifetime interact with existing place and ownership rules.
Do not represent references as ordinary copied `Value`s.

### Origin inference and call substitution

For each call:

- derive an actual origin from each argument place
- infer omitted origin and mutability parameters
- substitute actual origins into result/reference types
- derive field origins from projected places
- normalize unions by flattening and deduplicating members
- make a union mutable only when every member permits mutation
- retain the existing read/read versus read/write exclusivity rule using
  normalized origin overlap rather than only call syntax

The first implementation can omit wildcard and complex unsafe origins. Static
and untracked origins should be added only when a supported pointer or literal
API needs them.

### Lifetime extension analysis

Extend MIR ownership/liveness dataflow with reference dependencies:

- a live reference keeps every owner in its normalized origin live
- a union origin keeps all possible owners live
- field origins overlap their parent but can remain disjoint from sibling fields
- moving or destroying an owner while a reference is live is rejected
- returning a reference to a local owner is rejected
- branch joins merge possible origins conservatively
- loops reach a fixed point when reference state crosses a back-edge
- drop elaboration places owner destruction after the last dependent reference

This analysis should consume checked origin facts carried into MIR. It should
not reconstruct origins from source expressions or variable names.

### Reference-capable VM ABI

Design a runtime representation for references to variable slots and projected
places. It must support:

- mutation through a local reference binding
- references to fields and indexed elements where stable identity is valid
- passing references through multiple calls
- returning a reference tied to caller-owned storage
- parametric mutability
- invalidation consistent with statically checked moves and drops

The current frame-local `Vec<Value>` plus clone/write-back ABI does not provide
stable cross-frame aliases. Likely options include stable heap-allocated cells,
frame/slot handles with explicit caller relationships, or lowering verified
references into an addressable storage layer. Choose this only after the checked
origin model and lifetime analysis define exactly which aliases may survive.

### Origin conformance suite

Build the feature in vertical slices with positive and negative cases for:

- inferred immutable and mutable `ref` arguments
- an explicit named origin shared by an argument and return
- `origin_of(self.field)` and disjoint sibling fields
- a union return such as `-> ref[a, b] T`
- `ref name = collection[index]` mutation
- rejection of a returned reference to a local variable
- rejection of owner move/drop while a reference remains live
- owner destruction immediately after the final reference use
- exclusivity conflicts after origin substitution

### Work estimate and sequencing

Syntax support is small and present. Full semantics are **large**: they touch the
AST, generic parameter model, checked types, overload/call substitution, MIR,
CFG dataflow, drop elaboration, runtime storage, and the call ABI. Implement in
this order:

1. checked origin and reference types
2. local reference bindings to simple variable/field places
3. lifetime extension and invalid-escape diagnostics
4. inferred origins on `ref` arguments
5. origin-bearing ref returns and call substitution
6. union and field-sensitive origins
7. reference-capable VM behavior beyond simple places
8. unsafe/static/untracked origin forms only when demanded

Do not start by reproducing Mojo's complete internal origin attribute algebra.
The first useful stop point is a local or returned reference tied to a named
caller-owned place, with correct mutation, exclusivity, and destruction timing.

## Protocol Semantics

### Opaque indexing protocol

Define the associated index and result facts for `Indexer`. Teach the checker to
type `value[index]` for an opaque bounded type and route execution through
`__getitem__` without hardcoded container knowledge.

### Trait default methods

When a stdlib protocol requires one, preserve and type-check trait default
bodies in a `Self` context. Define override and multiple-default ambiguity rules,
then lower inherited bodies as ordinary checked methods visible to MIR and VM
metadata.

### Incremental Hasher protocol

Promote `Hasher` only for a concrete streaming or composite hashing use case.
Keep it separate from the existing deterministic `Hashable.__hash__() -> UInt`
contract.

### Writer and representation research

Verify current Mojo behavior for `Representable`, `Writable`, and `Writer` before
encoding semantics. Use one self-hosted formatter or writer as the acceptance
case and retain the existing pragmatic `print`/`__str__` path until then.

### Backend layout markers

Define `RegisterPassable` and `TrivialRegisterPassable` only alongside a backend
or ABI rule that can observe the difference. Add layout and call-boundary tests
with the implementation.

## Compile-Time Specialization

### Nested generic CTFE specialization

When demanded by stdlib code, specialize transitive CTFE helper calls per
compile-time argument tuple. The target case is `outer[T]()` calling `inner[T]()`
where the inner helper reads `T` facts. Preserve the shared fuel budget and add
recursion/cache tests.

### Richer compile-time values

Extend `CtValue` and VM materialization only for a concrete missing value shape.
Keep compile-time-only values from leaking into runtime MIR unless they have an
explicit materialization rule.

## Overload Growth

### Richer overload ranking

After rejection coverage is complete, collect real APIs that the current
exact/coercion scoring cannot express. Specify the ranking rules before changing
candidate selection and retain ambiguity as an error when no unique best match
exists.

### Generic overload ordering

Design ordering between generic and non-generic candidates, constrained type
parameters, and value-parameter specializations. Keep this independent from
ordinary numeric coercion scoring.

## Optional Experiments

### NIF exporter spike

Implement only the export-only experiment described in `docs/nif.md`: a small
NIF tree writer and a provisional `mojito-mir-v0` schema covering declarations,
functions, blocks, representative instructions, places, and terminators. Test
overloads, partial moves/drop elaboration, and try/finally escape flow. Do not add
an importer, BIF, indexes, or cache integration during the spike.

### Alternative backend boundary

Revisit an additional backend only after checked declarations and MIR are free
of backend-side AST reconstruction. Use the register VM as the executable
semantic oracle.
