# Mojito Roadmap

This is the project's single task tracker. It records the project's direction,
current capabilities, and a dependency-ordered list of unfinished work.
**Ordered Work** contains only pending tasks; completed tasks do not remain
there as checked boxes. Its first unchecked item is the recommended next
implementation task, and subsequent work proceeds from top to bottom.

The north star is self-hosting: useful standard-library code should expose the
next missing compiler capability. Prefer the smallest honest language change
that unlocks a real library pattern, with positive and negative tests.

## Current State

- [x] **Register VM pipeline** — HIR, MIR, ownership analysis, drop elaboration,
  and the register VM are the only execution path.
- [x] **Source modules and packages** — dotted, relative, qualified, aliased, and
  lexically scoped imports resolve through configurable roots; package
  `__init__.mojo` files re-export public declarations, while module-qualified
  internal identities prevent collisions after flat linking.
- [x] **Compile-time elaboration and CTFE** — compile-time constants and control
  flow, value specialization, type predicates, associated facts, and fuel-bounded
  VM execution work for the supported subset.
- [x] **Generic traits and associated facts** — user trait requirements,
  associated `comptime` members, iteration, comparison, sizing, hashing, numeric
  operation traits, and lifecycle marker traits have useful semantics.
- [x] **Signature-aware overloading** — functions, methods, and constructors use
  checker-selected overloads and canonical lowered symbols from `src/symbol.rs`.
- [x] **Mojo-shaped free-function arguments** — positional-only, keyword-only,
  defaults, keyword calls, and homogeneous `*args` work for ordinary functions.
- [x] **Self-hosted collection base** — `Optional`, `List`, `Set`, list-backed
  `Dict`, and experimental hash-backed `HashSet` are covered by self-host tests.
- [x] **Self-hosted algorithms and math** — generic iteration, direct compile-time
  facts, hashing helpers, and numeric rounding helpers exercise the compiler.
- [x] **Stabilization checkpoint** — strict local checks, named compiler records,
  canonical overload symbols, MIR declaration metadata, and actionable trait
  diagnostics are in place.
- [x] **Origin and reference semantics** — origin-bearing parameters, receivers,
  returns and unions use stable checked identities, persistent field-sensitive
  CFG loans, interprocedural substitution and escape checking, and executable
  frame/slot reference handles with captured projections.
- [x] **Overload rejection hardening** — duplicate, ambiguity, no-match,
  generic-ranking, bound-symbol, nested-def, and namespace regressions are pinned
  by the required overload rejection suite.
- [x] **Versioned CPU-parity ledger** — the Mojo manual inventory and pinned
  nightly target in `docs/mojo-nightly.md`
  classifies each feature family as parity, strict subset, divergence,
  representation difference, exclusion, or stretch, with validated evidence.
- [x] **Differential CPU conformance baseline** — shared fixtures exercise every
  implemented first-pass match plus representative subset and divergence edges
  against the pinned Mojo build; manifest validation requires executable evidence
  for parity and divergence claims.
- [x] **Core protocol contracts** — refined traits inherit requirements and
  capabilities; associated-type equalities compose; conditional conformances
  solve after specialization; and current Indexer, incremental Hasher, Writer,
  and Writable formatting contracts have checked, self-hosted proofs.
- [x] **Current lifecycle, conversion, and linearity semantics** — `@implicit`
  constructors participate in overload ranking and lower as explicit selected
  conversions. `imm` is the preferred
  immutable convention (`read` remains a compatibility spelling), ordinary
  values are implicitly deletable, and `ImplicitlyDeletable where False`
  creates explicit-destruction obligations independently of the required
  `@explicit_destroy("message")` diagnostic decorator. Obligations decompose
  into stable field paths after partial moves, linear fields can be destroyed
  independently, residual ordinary fields drop normally, and reconstructing all
  moved fields restores the whole-value destructor.
- [x] **Current constraints and scalar identity** — generic constraints use only
  trailing `where`, type predicates use `==`/`!=`, and pack-wide
  `conforms_to(Ts.values, Trait)` checks every heterogeneous type value. `Int`
  canonicalizes with `Scalar[DType.int]`; `SIMDSize` is a compile-time width
  parameter type and `_` infers construction width from explicit lanes.
- [x] **Current source imports, keyword forwarding, and slicing** — source
  packages win over same-named source modules, dotted imports bind every prefix,
  ordinary directories form namespace paths, and package members require an
  explicit import or initializer re-export. Homogeneous free-function keyword
  collectors are owned `StringDict[T]` values and forward with `**kwargs^`.
  Slice syntax selects `ContiguousSlice` or `StridedSlice`, preserves optional
  bounds, normalizes through `indices()`, and dispatches mixed or variadic
  `__getitem__`/`__setitem__` arguments through checked overloads.
- [x] **Generalized parameters and specialization** — type, value, origin,
  inferred, defaulted, dependent, and variadic parameters share one binder;
  trailing constraints, heterogeneous packs, nested specialization, structural
  cache keys, compile-time values, reflected type/declaration facts, and
  declaration-producing compile-time branches are checked before runtime
  lowering. The latest reflected-field handle spelling remains the first task
  below.
- [x] **Callable and closure completion** — overloaded and contextual generic
  callable values execute indirectly; explicit `unified { ... }` capture lists
  preserve immutable, mutable, moved, and reference captures; sibling calls,
  recursion, and non-escaping generic closures run without write-back emulation.
- [x] **CPU control and expression surface** — path-joined late initialization
  and implicit bindings, context managers, loop `else`, reference iteration,
  declaration destructuring, t-string interpolation, walrus expressions, and
  the remaining CPU operators have checked lowering and VM coverage. The
  remaining function-scope flow edge and owned iteration are listed below.
- [x] **Literal-family completion** — current numeric separators, radices,
  leading/trailing decimal points, exponents, raw strings, multiline strings,
  adjacent strings, and adjacent t-strings preserve their lexical and
  interpolation boundaries. Arbitrary-precision evaluation and lazy `TString`
  materialization remain explicitly scoped to the MIR-schema-prerequisite
  milestone below.
- [x] **Tuple and Variant completion** — bare and typed tuples, destructuring,
  compile-time indexing, structural operations, heterogeneous pack construction,
  and consuming operations run. `std.utils.Variant` has checked construction,
  membership tests, projection, mutation, consuming extraction, replacement,
  tag-aware places, moves, and element-wise protocol gating; additional library
  API breadth remains in the CPU standard-library milestone below.
- [x] **Reference aggregates** — direct and nested tuple/list reference storage,
  explicit-origin reference fields, aggregate moves, escape checks, owner-loan
  propagation, initialization, projection, and immutable-write rejection run
  through checked HIR/MIR and frame/slot handles. Direct `ref` fields are a
  documented Mojito extension; current Mojo's origin-bearing pointer aggregate
  model is the first remaining task below.
- [x] **Unsafe-pointer execution base** — pointer provenance, typed arithmetic,
  comparisons/conversions, alignment-aware allocation, explicit deallocation,
  dangling placeholders, and invalid/double-free diagnostics execute. Checked
  pointer types retain named, static, untracked, and unsafe-any origin kinds, and
  aggregate fields reject hidden unsafe-any origins.
- [x] **Typed checked boundary through HIR** — stable checked expression and
  declaration identities retain resolved types, binding/place categories,
  effects, origins, semantic adjustments, and recursively checked children. HIR
  carries those facts into CFGs and typed places; MIR no longer reconstructs
  semantics from source AST or source annotations.

## Ordered Work

Every entry is in implementation order. The first unchecked checkbox is the
default next task.

### 1. Close Remaining Nightly CPU Language Gaps

- [x] **Current reflected-field handles** — replace the legacy
  `Reflected.field_type[name]()` query with chainable `.field[name]` and
  `.field_at[index]` handles whose `.T` exposes the selected type. Migrate bundled
  code and turn the legacy spelling into a rejection case.
- [x] **Keyword collectors across call kinds** — support generic and method
  `**kwargs: T` collectors plus consuming `**kwargs^` forwarding through the
  same binder, specialization, duplicate detection, and origin/effect checks as
  ordinary free functions.
- [x] **Literal-default overload selection** — apply Mojo's contextual default
  type for otherwise-unconstrained integer and floating literals before declaring
  an overload set ambiguous, while preserving ambiguity for genuinely equivalent
  conversions.
- [x] **Function-scoped implicit-binding flow** — make an implicit binding
  introduced in a nested block visible throughout its function while retaining
  path-sensitive definite-initialization errors on paths that do not assign it.
- [x] **Trait-requirement effects** — carry `raises` and typed error facts through
  trait requirements, conforming methods, bounded dispatch, and selected calls so
  MIR effect verification receives the same contract as direct calls.
- [x] **Owned iteration** — implement `for var item in collection^` as consuming
  iteration over non-Copyable elements, including residual collection state,
  early exits, and conditional implicit-deletion/explicit-destroy obligations.
- [x] **Collection displays and comprehensions** — add set and dictionary
  displays plus CPU collection comprehensions with checked evaluation order,
  inference, ownership, and protocol-based construction.

### 2. Complete References And Unsafe Pointers

- [x] **Executable origin-bearing pointer loans** — infer the concrete source
  origin for `UnsafePointer(to=place)`, attach owner loans to aggregates that
  store such pointers, and reject dangling escape or conflicting access. The VM
  may erase origins after verification, but checked HIR/MIR must not. This is the
  remaining current-Mojo half of reference-aggregate parity and precedes the
  textual MIR/VM schema.

### 3. Finish Typed And Verified MIR

Do not add a separate rustc-style THIR stage. Strengthen the existing
`CheckedProgram` handoff into Mojito's typed semantic tree, then let HIR retain
that checked data while it makes statement control flow explicit. Source spans
remain diagnostic locations, not semantic identities. Declaration-wide facts
such as conformances, layouts, specialization records, and destruction policy
may remain indexed metadata.

- [ ] **Complete MIR value and instruction typing** — assign checked types to
  every synthetic register, constant, instruction result, call input/output, and
  effect edge. Preserve root, per-projection, and final-storage place types, and
  reject incomplete metadata without consulting source AST.
- [ ] **Complete standalone MIR semantic verification** — verify instruction and
  call types, CFG edges, ownership state, effects, and reference invariants from
  MIR plus checked declaration metadata alone. Keep the register VM as the
  executable specification and require production MIR to pass this verifier.

This milestone must complete before the textual MIR schema is frozen or native
backend work begins.

### 4. Complete MIR-Schema-Prerequisite CPU Semantics

These tasks may change MIR value, constant, or instruction schemas. Complete
them before freezing the textual format; API-only library growth follows it.

- [ ] **Protocolize scalar operations and conversions** — route numeric,
  comparison, conversion, and rounding behavior through checked protocols; add
  lossless arbitrary-precision `IntLiteral`/`FloatLiteral` storage and exact
  compile-time evaluation before contextual scalar materialization.
- [ ] **Protocolize collections and iteration** — route list/range/tuple indexing,
  sizing, containment, and iteration through the same contracts as user types.
- [ ] **Self-hosted Unicode String** — define storage, Unicode indexing/slicing,
  comparison, hashing, and formatting without VM-only semantics; distinguish
  compile-time `StringLiteral`, lazy captured `TString`, and explicit runtime
  `String` materialization.
- [ ] **CPU Layout and LayoutTensor semantics** — implement the target-independent
  type, indexing, and memory-view contracts required by CPU programs while
  leaving observable ABI layout and GPU memory spaces to later milestones.
- [ ] **SIMD semantic completion** — finish dtype/literal conversions, masks,
  reductions, shuffles, and other CPU-visible VM semantics.

### 5. Stabilize Textual MIR/VM Assembly

- [ ] **Backend-ready MIR checkpoint** — confirm that checked declarations plus
  typed verified MIR are sufficient inputs, with no source-AST reconstruction,
  before freezing a serialized schema.
- [ ] **Text format schema** — specify versioning, deterministic identifiers,
  declarations, blocks, instructions, constants, types, and source locations.
- [ ] **Disassembler** — print every verified MIR program deterministically and
  add stable snapshots for representative programs.
- [ ] **Assembler parser and diagnostics** — parse the text format with precise
  source errors and no dependency on Mojo source syntax.
- [ ] **Artifact verifier integration** — run the canonical MIR semantic verifier
  on assembled programs and report artifact source locations before execution.
- [ ] **Lossless round trips** — require MIR → text → MIR equivalence across the
  full test corpus.
- [ ] **VM artifact execution** — run verified textual artifacts directly from
  the CLI.
- [ ] **Compiler/test integration** — expose dumps and use assembly snapshots and
  conformance artifacts as backend-independent contracts.

### 6. Grow The CPU Standard Library

- [ ] **Collection API parity** — grow List, Dict, HashDict, Set, HashSet, tuple,
  slice, and optional/variant APIs demand-first from conformance cases. For
  `Variant`, finish `destroy_with`, representation writing, and fully generic
  TypeList-driven conditional protocol synthesis rather than adding compiler
  special cases for every standard-library method.
- [ ] **HashSet growth and rehashing** — add load-factor growth while preserving
  deterministic behavior and value semantics.
- [ ] **Filesystem and I/O slice** — port representative file/path/stream APIs on
  the Writer and explicit-destroy foundations.
- [ ] **Time, random, and testing slices** — add deterministic testable cores and
  isolate host-dependent behavior behind runtime services.

### 7. Packaging, Artifacts, And Developer Tooling

- [ ] **Feature and target options** — expose checked CLI/build configuration and
  record it in artifacts and diagnostics.
- [ ] **Compiled package artifacts** — define and load a versioned `.mojoc`
  representation without making modules first-class runtime values. Complete the
  per-directory resolution order around the already implemented source choices:
  source package, `.mojoc`, source module, then legacy `.mojopkg`.
- [ ] **Debugging metadata and inspection** — provide stack/source diagnostics,
  MIR inspection, and debugger-oriented value rendering.
- [ ] **Testing tools** — provide Mojito-native assertions, expected-error tests,
  and integration with the differential harness.
- [ ] **Distribution reproducibility gate** — make the release check rebuild,
  test, document, and reproduce conformance results using only the crates.io
  archive contents.

### 8. Native Backends And Native-Only Semantics

- [ ] **Cranelift scalar backend** — lower the verified scalar CPU subset and
  validate it differentially against the VM/textual corpus.
- [ ] **Observable CPU layout and ABI rules** — define size, alignment, field
  layout, calling convention, and layout-marker semantics against native output;
  this is intentionally not a VM-parity prerequisite.
- [ ] **Cranelift SIMD lowering** — map completed SIMD semantics to native vectors
  where supported, retaining scalar fallback behavior.
- [ ] **LLVM backend** — share the verified MIR contract and add stronger
  optimization/vectorization coverage.
- [ ] **Stretch backends** — investigate eBPF and MLIR only after Cranelift and
  LLVM are stable; neither is a first-pass parity requirement.

### Explicit Non-Goals For First-Pass Parity

- GPU programming and accelerator memory/execution models
- concurrency, parallelism, atomics, tasks, and distributed execution
- Python interoperability
- MLIR as a required compiler layer or backend
- legacy `fn`, `owned`, and other removed source spellings except for clear
  rejection diagnostics
- escaping closures and the removed `escaping` function effect; first-pass
  closure parity targets Mojo's current non-escaping capture-list model

## Task Lifecycle Policy

`roadmap.md` is the only task list. Do not create a parallel todo file.

- Unfinished work belongs in **Ordered Work** as an unchecked, outcome-oriented
  task. Add detailed design notes elsewhere only when they are needed to make a
  decision or preserve an architectural argument.
- A task is complete only when its implementation, focused positive and negative
  coverage, relevant documentation, and `scripts/check` all agree.
- In the same change that completes a task, remove it from **Ordered Work**. Add
  or update one brief capability entry in **Current State** only when it changes
  the useful high-level picture.
- Record user-visible release history in `CHANGELOG.md`; record lasting design
  invariants in `docs/architecture.md` or the relevant focused document. Delete
  obsolete implementation plans instead of retaining them as completed todos.
- Split or rewrite partially completed tasks so **Ordered Work** states only the
  remaining outcome. Never mark a broad task complete while leaving hidden
  follow-up work inside its description.
- Prefer one checkbox per independently demonstrable semantic outcome. Split a
  task when its parts require different compiler phases, can land without one
  another, have different backend dependencies, or need distinct conformance
  cases. A task may still span phases when those phases are inseparable from one
  end-to-end language guarantee.

## Working Rule

For each promoted task:

1. Start with a self-hosted library or small user-facing acceptance case.
2. Record the current failure with a focused test.
3. Implement the smallest compiler change that makes the program honest.
4. Add positive and negative coverage at the owning compiler phase.
5. Run `scripts/check` before marking the task complete.

Deferred work stays unchecked. Completion follows the lifecycle policy above.
