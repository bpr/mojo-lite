# Mojito Roadmap

This is the project's single task tracker. It records the project's direction,
current capabilities, and a mostly dependency-ordered list of unfinished work.
The ordered list contains only pending or demand-driven tasks; completed tasks
do not remain there as checked boxes.

The north star is self-hosting: useful standard-library code should expose the
next missing compiler capability. Prefer the smallest honest language change
that unlocks a real library pattern, with positive and negative tests.

## Current State

- [x] **Register VM pipeline** — HIR, MIR, ownership analysis, drop elaboration,
  and the register VM are the only execution path.
- [x] **Module linking and stdlib roots** — dotted and relative imports,
  transitive linking, configurable search roots in the library API, and
  `stdlib/std/...` imports work.
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

## Ordered Work

The order below expresses dependencies, not a promise that every item must be
implemented. Demand-driven items should be promoted only when a concrete stdlib
or user program needs them.

### 1. Define And Measure CPU-Language Parity

- [ ] **Versioned parity manifest** — pin the target Mojo release and classify
  every manual feature as implemented, partial, excluded, or stretch. First-pass
  exclusions are GPU, concurrency/parallelism, distributed execution, Python
  interoperability, and MLIR.
- [ ] **Differential conformance harness** — run focused programs under Mojo and
  Mojito, recording matching results, matching rejection, and documented
  intentional divergence.

### 2. Complete Modules And Packages

- [ ] **Module namespaces** — implement qualified `import module`, aliases,
  package boundaries, visibility/re-exports, and imported initialization while
  preserving overload and declaration identity across modules.

### 3. Complete Functions, Calls, And Effects

- [ ] **Full call model** — named `out` results, ordinary `out`, static methods,
  heterogeneous variadics and parameter packs, callable expressions, and parity
  across free, method, constructor, and generic calls.
- [ ] **Mojo overload ordering** — implicit constructors/conversions,
  convention-aware filtering, variadic preference, parameter-signature ranking,
  and generic-versus-non-generic ordering with ambiguity retained as an error.
- [ ] **Raising effects and typed errors** — enforce `raises`, propagation and
  handling; add typed/parametric errors, `Never`, and their function-type and
  overload interactions.

### 4. Complete Lifecycle And Traits

- [ ] **Lifecycle closure** — named-result initialization, non-movable return
  construction, raising lifecycle methods, explicitly destroyed types, and
  definite initialization across branches and exceptional flow.
- [ ] **Trait model** — refinement, default bodies, override/ambiguity rules,
  associated types/constants, compositions, conditional conformance, and
  compile-time capability refinement.
- [ ] **Core protocols** — opaque `Indexer` dispatch; demand-driven incremental
  `Hasher`; `Writer`, `Writable`, and `Representable` semantics demonstrated by
  a formatter; and layout markers tied to observable native ABI rules.

### 5. Generalize Parameterization And Compile-Time Execution

- [ ] **General parameters and packs** — arbitrary compile-time parameter types,
  variadic type/value packs, dependent parameter expressions, constraints, and
  full parameter binding rules.
- [ ] **Specialization engine** — nested generic CTFE helper specialization,
  caching, shared fuel and recursion diagnostics, richer compile-time values with
  explicit materialization rules, reflection, and declaration generation where
  conformance cases require them.

### 6. Complete Closures And Remaining Language Surface

- [ ] **Callable values and closure environments** — escaping and non-escaping
  closures, inferred/explicit captures with ownership conventions, indirect MIR
  invocation, nested/sibling/recursive/generic local functions, and captured
  reference lifetime checking.
- [ ] **Control and expression closure** — context managers, t-strings, walrus,
  complete slicing, chained comparisons, destructuring patterns, reference loop
  bindings, and remaining CPU-relevant operators.
- [ ] **Advanced reference/pointer boundary** — static/untracked/unsafe origins,
  reborrows, reference aggregates where valid, pointer provenance, allocation,
  deallocation, alignment, and typed address arithmetic.

### 7. Stabilize A Textual MIR/VM Assembly Format

- [ ] **Assembler/disassembler** — define a deterministic, human-readable,
  flattened and versioned Mojito MIR/VM format with parser, printer, verifier,
  source diagnostics, lossless round trips, and VM execution.
- [ ] **Artifact and test integration** — use the text form for MIR snapshots,
  compiler dumps, conformance artifacts, and native-backend differential tests;
  consider compact bytecode only after the textual schema stabilizes.

### 8. Reduce Builtins And Grow The CPU Standard Library

- [ ] **Protocol-driven builtins** — route scalar, string, collection, range,
  tuple, SIMD, conversion, formatting, hashing, indexing, and iteration behavior
  through the same checked protocol model as user types.
- [ ] **CPU stdlib growth** — port representative core, collection, string,
  formatting, algorithms, math, filesystem/I/O, time, random, and testing slices.
- [ ] **HashSet growth and rehashing** — preserve the existing concrete
  self-hosted acceptance case within this broader standard-library phase.
- [ ] **SIMD semantic completion** — complete CPU-relevant SIMD typing and
  behavior in the VM; native vector lowering is a backend goal, not a condition
  for VM semantic parity.

### 9. Native Backends

- [ ] **Backend-ready MIR** — remove remaining source-AST reconstruction and make
  checked declarations, verified MIR, and textual assembly sufficient inputs.
- [ ] **Cranelift backend** — first native CPU backend, validated differentially
  against the VM and textual assembly corpus.
- [ ] **LLVM backend** — second native CPU backend, sharing the same verified MIR
  contract and adding stronger optimization/vectorization opportunities.
- [ ] **Stretch backends** — investigate eBPF and MLIR only after Cranelift and
  LLVM are stable. They are not first-pass parity requirements.

### Explicit Non-Goals For First-Pass Parity

- GPU programming and accelerator memory/execution models
- concurrency, parallelism, atomics, tasks, and distributed execution
- Python interoperability
- MLIR as a required compiler layer or backend

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

## Working Rule

For each promoted task:

1. Start with a self-hosted library or small user-facing acceptance case.
2. Record the current failure with a focused test.
3. Implement the smallest compiler change that makes the program honest.
4. Add positive and negative coverage at the owning compiler phase.
5. Run `scripts/check` before marking the task complete.

Deferred work stays unchecked. Completion follows the lifecycle policy above.
