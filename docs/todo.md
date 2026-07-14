# Todo

This file contains only unfinished, concrete work. Phase order and completed
milestones belong in `roadmap.md`; architectural invariants belong in
`docs/architecture.md`.

Tasks are grouped in the same approximate dependency order as the roadmap.

## Self-Hosted Collections

### HashSet growth and rehashing

Port `HashDict`'s explicit bucket growth to the self-hosted `HashSet`; retain
collision behavior and deep-copy semantics while rebuilding nested-list buckets.

## Origins, References, and Lifetimes

Mojo origins are compile-time symbolic descriptions of the storage that governs
a reference and of the mutability available through it. They are used to extend
owner lifetimes, enforce exclusivity, and reject references that outlive their
owners. They have no runtime identity and are erased after lifetime checking.

Mojito retains the principal surface forms and implements the safe caller-owned
subset. Stable checked owner IDs and a canonical origin algebra feed local and
cross-call aliases. MIR records persistent loans, uses CFG liveness to end them
at last use, and rejects overlapping owner access while they remain live.
The remaining work is demand-driven unsafe/static origin support:

- MIR places already identify rooted field/index projections.
- Call checking already rejects overlapping mutable/shared argument places and
  permits statically disjoint fields.
- MIR places record whether access occurs directly or through a local loan.
- Ownership analysis already tracks moves through CFG joins and loops.
- Liveness and drop elaboration already insert ASAP destruction and edge cleanup.
- reference-bearing parameters and returns use persistent frame/slot identity.

Statically resolvable local aliases compile to verified owner-place accesses;
cross-call aliases materialize frame/slot handles that continue to identify
caller storage after the callee returns.

### Origin representation — foundation complete

Implemented source/checked foundations include:

- origin expressions for named origins, `origin_of(place)`, arbitrary
  `origin_of` expressions, `_`, and unions
- origin parameters with immutable, mutable, or parameterized mutability
- the `//` infer-only boundary in generic parameter declarations
- origin-bearing `ref` parameter and return types
- source spans for origin diagnostics

The canonical algebra has stable owner/origin-parameter IDs, projected places,
normalized unions, static/untracked placeholders, and explicit mutability.
Signature clauses lower into this algebra during checking.

### Local reference binding semantics — complete

`RefTy` contains referent type, origin, and mutability. `ref name = place` is an
alias rather than an owned binding and accepts variable, field, and indexed
places. Reads auto-dereference, writes target the owner, and index operands are
frozen at binding time.

Local references use owner-place operations where statically resolvable and
frame/slot handles where dynamic identity crosses a call.

### Origin inference and call substitution — complete

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

### Lifetime extension analysis — complete for caller-owned origins

Implemented for local references:

- a live reference keeps every owner in its normalized origin live
- field origins overlap their parent but can remain disjoint from sibling fields
- moving or destroying an owner while a reference is live is rejected
- branch joins merge possible origins conservatively
- loops reach a fixed point when reference state crosses a back-edge
- drop elaboration places owner destruction after the last dependent reference

Union/substituted owners and return escape rejection are now checked as callable
origin contracts are lowered and substituted.

### Reference-capable VM ABI — core complete

Runtime references are shallow `{ frame, slot, projection }` handles. Field
names and evaluated list indexes are captured into the handle, and stale frame
identities fail loudly. The implemented core supports:

- mutation through a local reference binding
- references to fields and indexed elements where stable identity is valid
- passing references through multiple calls
- returning a reference tied to caller-owned storage
- parametric mutability
- invalidation consistent with statically checked moves and drops

MIR exposes `MakeRef`, `ReadRef`, and `WriteRef` as the representation-neutral
boundary. Direct reference-producing calls pass handles instead of copying and
writing back, and returned handles keep the dynamically selected caller place.

### Origin conformance suite — complete

The fixture suite contains positive and negative cases for:

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
CFG dataflow, drop elaboration, runtime storage, and the call ABI. The checked
foundation, persistent-loan analysis, parameter and return substitution, VM
frame stack, reference ABI, and conformance consolidation are complete.
Unsafe/static/untracked origin forms remain deferred until a concrete pointer or
literal API requires them.

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
