# Mojito Roadmap

This roadmap records the project's direction and a mostly dependency-ordered
task list. Checked entries are complete and intentionally brief. Unchecked
entries are pending or demand-driven.

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
- [x] **Origin surface syntax** — `ref[origin]` arguments and returns, origin
  unions, named `Origin[mut=...]` parameters, infer-only `//`, and `ref` bindings
  parse and retain their clauses for semantic resolution.
- [x] **Checked origin foundation and local references** — stable owner IDs,
  canonical origins/unions, executable local `ref` aliases, bind-time index
  evaluation, and persistent field-sensitive CFG loans are in place.
- [x] **Overload rejection hardening** — duplicate, ambiguity, no-match,
  generic-ranking, bound-symbol, nested-def, and namespace regressions are pinned
  by the required overload rejection suite.

## Ordered Work

The order below expresses dependencies, not a promise that every item must be
implemented. Demand-driven items should be promoted only when a concrete stdlib
or user program needs them.

### 1. Finish Self-Hosted Collection Growth

- [ ] **HashSet growth and rehashing** — port `HashDict`'s explicit bucket growth
  to the self-hosted `HashSet`, preserving collision behavior and deep-copy
  semantics while rebuilding nested-list buckets.

### 2. Add Origin and Reference Semantics

Mojo origins symbolically identify the storage governing a reference and whether
that reference permits mutation. They extend owner lifetimes, constrain mutable
aliasing, and disappear before runtime code generation. Mojito implements the
safe caller-owned subset with persistent CFG loans, cross-call substitution,
reference returns, and frame/slot runtime handles. Unsafe, static, and untracked
origin forms remain demand-driven extensions. See the implementation notes in
`docs/todo.md` and the [Mojo lifetime manual](https://mojolang.org/docs/manual/values/lifetimes/).

- [x] **Origin representation foundation** — origin clauses and infer-only
  parameters are retained; checked origins use stable owner/parameter IDs,
  normalized unions, projection paths, and explicit mutability.
- [x] **Local reference binding semantics** — `ref name = place` aliases variable,
  field, and indexed storage without copying; MIR loans enforce exclusivity and
  owner lifetime through the reference's CFG last use.
- [x] **Origin-checked parameters and receivers** — validate named and
  place-derived parameter origins, keep `Origin` parameters semantic-only,
  check fixed/parametric mutability in generic bodies, and execute `ref self`.
- [x] **Origin-bearing return reference types** — checked `ref T` returns require
  origins; return sites are checked for declared-origin subsumption and local
  escapes, including receiver fields and unions.
- [x] **Origin inference and substitution** — argument/receiver places become
  projected origins, unions normalize, callee contracts substitute at calls,
  and solved immutable references permit shared aliases.
- [x] **Interprocedural lifetime and escape analysis** — substituted and union
  owners extend through a returned reference's last use; overlap, moves, calls,
  and escaping local return origins are rejected.
- [x] **Explicit VM frame stack** — function calls use monotonic frame IDs,
  heap-owned register/variable frames, return continuations, and iterative
  dispatch; deep source recursion no longer consumes the Rust call stack.
- [x] **Reference-capable VM ABI** — runtime references are shallow frame/slot
  handles with captured field/index projections; reference-producing calls,
  `ref self`, union returns, and writes through returned aliases execute.
- [x] **Origin conformance suite** — covers immutable/mutable inference, unions,
  disjoint fields, ref returns, premature destruction, and invalid escapes.

### 3. Expand Protocol Semantics When Libraries Need Them

- [ ] **Opaque indexing protocol** — define the index and result associated facts
  needed for `T: Indexer`, then type generic subscripting without container
  special cases.
- [ ] **Trait default methods** — type-check and lower inherited default bodies,
  including ambiguity rules, only when a real protocol needs shared behavior.
- [ ] **Incremental Hasher protocol** — add `Hasher` only when streaming or
  composite hashing needs more than the current `__hash__() -> UInt` contract.
- [ ] **Writer and representation research** — verify current Mojo semantics for
  `Representable`, `Writable`, and `Writer`, then implement the smallest protocol
  needed by a self-hosted formatting use case.
- [ ] **Backend layout markers** — define `RegisterPassable` and
  `TrivialRegisterPassable` only when a backend or ABI can observe the
  distinction.

### 4. Deepen Compile-Time Specialization on Demand

- [ ] **Nested generic CTFE specialization** — specialize transitive helper calls
  by compile-time argument tuple when stdlib code needs `outer[T]()` to call an
  `inner[T]()` that reads type facts.
- [ ] **Richer compile-time values** — extend `CtValue` and materialization only
  for concrete associated-value, declaration-generation, or specialization use
  cases.

### 5. Grow Overloading Only With Evidence

- [ ] **Richer overload ranking** — extend the current exact-versus-coercion model
  only after negative coverage is strong and real APIs require more ranking
  rules.
- [ ] **Generic overload ordering** — define ordering between generic and
  non-generic candidates, constraints, and value-parameter specializations as a
  separate design task rather than an accidental extension of coercion scoring.

### 6. Optional Interchange and Backend Experiments

- [ ] **NIF exporter spike** — prototype an export-only `mojito-mir-v0` subset for
  representative MIR programs and evaluate readability, fidelity, and tooling
  value before considering import or cache support; see `docs/nif.md`.
- [ ] **Alternative backend boundary** — add another backend only after checked
  declarations and MIR contain enough semantics that it does not need to
  reconstruct language rules from the AST.

## Working Rule

For each promoted task:

1. Start with a self-hosted library or small user-facing acceptance case.
2. Record the current failure with a focused test.
3. Implement the smallest compiler change that makes the program honest.
4. Add positive and negative coverage at the owning compiler phase.
5. Run `scripts/check` before marking the task complete.

Deferred work stays unchecked. When a task is completed, reduce it to one brief
checked entry in the **Current State** section or its relevant roadmap section;
remove its detailed implementation notes from this file and remove the task
entirely from `docs/todo.md`.
