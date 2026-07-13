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
  parse; origin clauses are not yet given semantic meaning.
- [x] **Overload rejection hardening** — duplicate, ambiguity, no-match,
  generic-ranking, bound-symbol, nested-def, and namespace regressions are pinned
  by the required overload rejection suite.

## Ordered Work

The order below expresses dependencies, not a promise that every item must be
implemented. Demand-driven items should be promoted only when a concrete stdlib
or user program needs them.

### 1. Close Small Correctness and Interface Gaps

- [x] **Overload rejection coverage** — 46 required tests cover duplicate
  signatures, ambiguity, no-match behavior, generic ranking, bound-aware
  symbols, method/constructor diagnostics, and namespace collisions; the fixed
  findings are retained in `overload_errors.md`.
- [x] **CLI module search paths** — repeatable `--module-path`/`-I` and
  `--stdlib` roots are searched after the importer directory, in command-line
  order, before the bundled stdlib fallback.
- [x] **Method argument binding parity** — ordinary method calls use the same
  keyword, default, positional-only, keyword-only, and variadic binding rules as
  free functions while preserving receiver and ordinary-parameter write-back.
- [x] **Generic call binding parity** — generic free functions use the same
  default, keyword, positional-only, keyword-only, variadic, convention, and
  alias-aware argument binding as non-generic functions.
- [x] **Diagnostic-mode parser recovery** — normal compilation remains fail-fast;
  the parse CLI uses a capped, statement-synchronized diagnostic mode that
  reports multiple spanned errors and quarantines its partial AST.

### 2. Strengthen Self-Hosted Collections

- [x] **Nested self-hosted lists** — pointer-backed element reads deep-copy values,
  indexed mut-self writeback uses `__setitem__`, and `HashSet` now uses the
  self-hosted `List[List[T]]` bucket shape.
- [x] **Dictionary views** — `Dict` provides insertion-ordered key iteration,
  public `DictEntry` items, snapshot `keys`/`values`/`items`, and Mojo-shaped
  overloaded `get` accessors.
- [x] **Hash-backed dictionary** — `HashDict` combines dense insertion-ordered
  entries with nested-list hash indexes, explicit growth/rehashing, snapshots,
  overloaded accessors, and deep value-semantic copying.
- [x] **Keyword-map value** — homogeneous free-function `**kwargs: T` uses ordered
  keyword pairs as its call ABI and materializes an owned self-hosted
  `HashDict[String, T]` in the callee; representation rationale is retained in
  `docs/kwargs_dict_memo.md`.

### 3. Centralize Checked Semantic Data

- [ ] **Checked program representation** — introduce a `CheckedProgram` or
  equivalent checked-declaration table containing resolved types, symbols,
  conformances, and callable metadata instead of returning only checker side
  tables.
- [ ] **Typed MIR declarations** — migrate the remaining AST `Type` and `Expr`
  values in `MirDeclarations` to checked types and normalized constants derived
  from the checked program.
- [ ] **Module identity preservation** — preserve declaration provenance beyond
  linking when needed for diagnostics, incremental compilation, or external
  interchange; avoid changing the current flat semantics without a concrete
  consumer.

### 4. Add Origin and Reference Semantics

Mojo origins symbolically identify the storage governing a reference and whether
that reference permits mutation. They extend owner lifetimes, constrain mutable
aliasing, and disappear before runtime code generation. Mojito currently has
call-scoped, place-sensitive exclusivity and ASAP drop analysis, but it does not
have origin-carrying reference values. This is a multi-stage compiler and VM
project, not a checker-only feature. See the detailed assessment in
`docs/todo.md` and the [Mojo lifetime manual](https://mojolang.org/docs/manual/values/lifetimes/).

- [ ] **Origin representation** — preserve origin clauses and infer-only origin
  parameters in the AST and checked types instead of discarding them.
- [ ] **Reference type and binding semantics** — model `ref T` values and
  `ref name = place` without copying the referent.
- [ ] **Origin inference and substitution** — infer argument origins, derive
  field origins, normalize unions, and substitute callee origins at call sites.
- [ ] **Lifetime extension analysis** — keep every owner in a reference's origin
  live through the reference's last use and reject escaping local origins.
- [ ] **Reference-capable VM ABI** — represent aliases to caller storage across
  calls and ref returns; replace write-back emulation where true reference
  identity is required.
- [ ] **Origin conformance suite** — cover immutable/mutable inference, unions,
  disjoint fields, ref returns, premature destruction, and invalid escapes.

### 5. Expand Protocol Semantics When Libraries Need Them

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

### 6. Deepen Compile-Time Specialization on Demand

- [ ] **Nested generic CTFE specialization** — specialize transitive helper calls
  by compile-time argument tuple when stdlib code needs `outer[T]()` to call an
  `inner[T]()` that reads type facts.
- [ ] **Richer compile-time values** — extend `CtValue` and materialization only
  for concrete associated-value, declaration-generation, or specialization use
  cases.

### 7. Grow Overloading Only With Evidence

- [ ] **Richer overload ranking** — extend the current exact-versus-coercion model
  only after negative coverage is strong and real APIs require more ranking
  rules.
- [ ] **Generic overload ordering** — define ordering between generic and
  non-generic candidates, constraints, and value-parameter specializations as a
  separate design task rather than an accidental extension of coercion scoring.

### 8. Optional Interchange and Backend Experiments

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
