# Proposed Chapter Layout for *Mojito Internals*

## Editorial direction

The book should explain Mojito as a sequence of representations and invariants, not as a guided tour of source files. A reader should finish each part knowing what information exists at that stage, which earlier information has deliberately been discarded, and what guarantees the next stage may rely on. Source files remain important, but they should appear as concrete implementations of concepts rather than dictate the narrative structure.

The syntax frontend should be intentionally short. Lexing and parsing are necessary context, but most of Mojito's interesting engineering begins after parsing: module linking, compile-time elaboration, the Mojo-shaped type system, traits and overload resolution, control-flow lowering, ownership, destruction, and execution in the register VM. The chapter balance below reflects that emphasis.

The current documentation provides a strong starting point. `docs/frontend.md` and `grammar.md` supply the compact frontend material; `docs/architecture.md` supplies the pipeline spine; `docs/notes/comptime.md` covers compile-time execution; `docs/notes/consolidate_overloading.md` provides a useful case study in cross-stage identity; `roadmap.md` explains why the type and trait system developed in its present order; and `CLAUDE.md` is a dense implementation index that can be mined for details and examples. Planning documents should inform historical or forward-looking sidebars, but the main text must clearly distinguish implemented behavior from proposed work.

## Part I — Orientation

### 1. What Mojito Is

Introduce Mojito as a small Rust implementation of a practical Mojo subset whose primary purpose is to explore compiler architecture and self-hosting. Establish what “Mojo-compatible” means here, what is intentionally unsupported, why the tree walker was retired, and why the register VM is now the authoritative execution path. This chapter should also explain the intended reader: someone comfortable with programming and basic compiler terminology, but not assumed to know Mojo or Rust compiler internals.

### 2. The Compilation Journey

Present the complete pipeline at a glance: source text, tokens, AST, linked program, comptime-elaborated AST, checked types, HIR control-flow graph, flattened MIR, ownership and liveness transformations, and VM execution. Use one tiny program and show its changing shape without explaining every field yet. The goal is to give readers a durable map that later chapters repeatedly revisit.

### 3. How to Read and Experiment with the Compiler

Explain the repository layout, the `lex`, `parse`, `check`, `own`, and `run` CLI stages, and the role of `scripts/check`. Introduce file-based fixtures and focused integration tests as executable specifications. Include a recommended workflow for following a value or construct through the compiler using `rg`, debug output, MIR tests, and small `.mojo` fixtures.

## Part II — The Compact Syntax Frontend

### 4. Tokens, Layout, and Mojo's Python-Like Surface

Describe the lexer, token spans, indentation stacks, newline suppression inside delimiters, comments, literals, keywords, and stropped identifiers. Pay special attention to the offside rule because it is the one frontend mechanism that materially affects later block structure. Docstrings, t-strings, numeric spellings, and the boundary between hard keywords and contextual words can be treated as focused examples rather than separate chapters.

### 5. Parsing into the AST

Explain the recursive-descent/precedence parser, statement suites, expressions, places, type annotations, generic parameter syntax, and how spans propagate into AST nodes. Show how the AST preserves source-level distinctions that matter later—`VarDecl` versus `Assign`, `SetPlace` versus ordinary assignment, receiver conventions, and parameter markers—while deferring semantic questions to the checker.

### 6. Grammar as an Executable Contract

Use `grammar.md` to explain how Mojito documents its accepted surface language and how syntax changes should be developed. Cover the difference between parsing a construct and supporting its semantics, including “parse now, reject cleanly later.” This chapter should be short and practical: adding a syntax feature means updating the grammar, lexer/parser tests, AST, and an explicit downstream support decision.

## Part III — From Files to a Checked Program

### 7. Modules, Linking, and Declaration Hoisting

Explain how `src/module.rs` resolves relative and absolute imports, applies search roots, loads transitive modules, prevents duplicate loading, and produces the flat program consumed by later stages. Discuss why this is an implemented foundation rather than a full module runtime, how imported overload sets must remain intact, and how the bundled `stdlib/std/...` layout influences resolution.

### 8. Compile-Time Elaboration

Describe the comptime phase as an AST-to-AST transformation that resolves compile-time constants and supported compile-time control flow before ordinary checking and lowering. Cover direct integer evaluation, type/value parameters, specialization inputs, fuel, and VM-backed CTFE. Emphasize the phase boundary: runtime stages should see the elaborated result rather than repeatedly rediscover compile-time facts.

### 9. The Checker's World Model

Introduce the central checker state: lexical scopes, mutability scopes, function boundaries, generic type-parameter scopes, `Self` context, struct and trait registries, comptime facts, and resolved-callee tables. Explain why these are separate namespaces and stacks rather than one universal symbol table. This chapter establishes the data structures readers need before studying individual type rules.

## Part IV — The Mojito Type System

### 10. Source Types and Checked Types

Contrast `ast::Type`, which preserves annotations as written, with `types::Ty`, which represents semantic types after resolution. Walk through scalars, literal types, lists, tuples, structs, SIMD types, pointers, function types, type parameters, associated types, and `Self`. Explain materialization and why a numeric literal type remains flexible until context forces a concrete representation.

### 11. Type Arguments, Generic Declarations, and Substitution

Explain `ParamDecl`, `TyArg`, type parameters, value parameters, bounds, and the distinction between erased type arguments and reified compile-time value arguments. Show how unification solves generic calls and constructors, how substitution specializes fields and method signatures, and why value arguments form part of a struct type's identity even though dependent field types remain out of scope.

### 12. Numeric Types, Literals, and SIMD

Cover Mojito's scalar numeric model, literal coercion, strict separation of concrete numeric types, conversions, true division, floor division, and fixed-width SIMD lanes. Explain the special relationship between scalar aliases and width-one SIMD types, lane materialization, wrapping behavior, and why numeric operator typing must remain consistent across checker, runtime helpers, and VM instructions.

### 13. Struct Types and Value Semantics

Describe field registration, `@fieldwise_init`, hand-written `__init__`, definite initialization, method registries, generic structs, and runtime representation. Explain Mojito's value semantics: assignment and ordinary argument passing copy values when permitted, mutation targets a binding or place, and lifecycle methods may make copying, moving, and destruction observable.

### 14. Functions, Parameters, and Call Shapes

Explain `Ty::Func` and generic function signatures, required/default parameters, positional-only and keyword-only markers, homogeneous variadics, argument conventions, and slot matching. Show how the same call-shape logic must agree in checker and VM. Clearly separate implemented free-function behavior from the remaining method, generic-call, and `**kwargs` gaps.

### 15. Traits, Bounds, and Associated Facts

Introduce built-in and user-defined traits, nominal conformance, method requirements, receiver conventions, associated compile-time values/types, and opaque bounded type parameters. Organize the implemented trait families by capability—equality, ordering, hashing, sizing, numeric operations, iteration, and lifecycle markers—and explain how a bound grants operations without exposing a concrete representation.

### 16. Methods, `Self`, and Receiver Conventions

Focus on method lookup and the special role of the receiver. Cover read-only `self`, `mut self`, lifecycle receivers such as `out` and `deinit`, substitution of `Self` and `Self.T`, receiver-place requirements, and mutation write-back. Explain why method calls combine type lookup, place analysis, and runtime dispatch in ways ordinary free calls do not.

### 17. Overload Sets and Canonical Symbols

Explain how repeated declarations form overload sets, how candidates are filtered and scored, why ambiguity is rejected, and how the selected declaration crosses stage boundaries. Use `src/symbol.rs` as a case study in compiler-wide identity: AST annotations and checked types must generate the same collision-safe lowered symbol, including generic/value arguments and stropped names. Discuss exact resolved-callee tables, MIR function naming, synthetic VM fallback paths, and the bugs caused by duplicated mangling logic.

### 18. Places, Mutation, and Borrow Rules

Define a place as a rooted storage location plus field/index projections. Show how the checker validates writes, mutating list operations, `mut self`, and `mut`/`ref` arguments. Explain the call-scoped, place-sensitive aliasing rule: disjoint fields may be borrowed independently, while a whole object overlaps every subplace and dynamic indices are conservatively assumed to alias.

### 19. Errors and Unsupported Semantics

Survey lexical, parse, type, ownership, and runtime error categories and explain why phase-specific diagnostics matter. Show how unsupported-but-parsed constructs fail deliberately instead of leaking into later stages. Include guidance for designing new diagnostics that preserve context, especially for failed trait bounds, bad calls, mutability violations, and ownership errors.

## Part V — Control Flow and Intermediate Representations

### 20. Why Mojito Uses Two IR Levels

Explain the division of labor between HIR and MIR. HIR exposes control-flow structure while retaining nested AST expressions; MIR flattens expressions into registers and explicit operations suitable for analysis and execution. Discuss why performing both transformations at once would entangle control-flow construction, expression evaluation order, source spans, and ownership reasoning.

### 21. Building the HIR Control-Flow Graph

Walk through basic blocks, terminators, edges, variable interning, lexical shadow slots, and lowering cursors. Show the shapes produced for `if`, `while`, `for`, `break`, `continue`, and returns. Explain sealed blocks and why dead statements after a terminator are omitted.

### 22. Exceptions and Nested Regions

Explain how `try` bodies, handlers, `else`, and `finally` complicate ordinary CFG structure. Cover region CFGs, seeded variable slots, escaping loop targets, `EscapeJump`, and the distinction between control flow local to a region and flow that must propagate to an enclosing function driver.

### 23. Flattening Expressions into MIR

Describe A-normal-style flattening: every nested expression becomes an ordered sequence of register-producing instructions. Cover constants, unary and binary operations, short-circuiting, calls, member/index access, places, and source-span tracking. Show how flattening makes evaluation order explicit and prevents repeated evaluation of augmented-assignment targets.

### 24. MIR Program, Blocks, and Instructions

Catalog the core MIR data structures: `MirProgram`, functions, blocks, registers, variable slots, instructions, places, and terminators. Rather than listing every enum variant mechanically, group instructions by purpose—values, storage, calls, iteration, mutation, ownership, exceptions, and cleanup—and state the invariant each group gives later analyses and the VM.

### 25. Calls, Closures, and Nested Function Lifting

Explain how free calls, methods, constructors, defaults, keyword slots, and compile-time arguments are represented in MIR. Then cover nested `def` capture analysis, lifted names, capture parameters, mutable write-back, immutable capture shadowing, and the limits of the current closure model. This chapter should connect source-level lexical scope to concrete function frames.

## Part VI — Ownership, Lifetime, and Destruction

### 26. Mojito's Ownership Model

Introduce owned, moved, borrowed, and reinitialized states and relate them to Mojo conventions and the `^` transfer sigil. Explain which types are copyable, how lifecycle traits affect legality, and why ownership is checked after MIR lowering rather than entirely during type checking.

### 27. Ownership as Dataflow

Present the forward dataflow analysis over MIR blocks: state entering a block, instruction transfer functions, joins, loop convergence, and diagnostics for use after move or conditional move. Use small CFG diagrams to demonstrate why straight-line bookkeeping is insufficient once branches and loops appear.

### 28. Partial Moves and Place Trees

Explain the tree-shaped state needed to track `p.a^` independently from `p.b`. Cover whole-value versus subplace state, reinitializing moved fields, conservative indexed moves, and the conditions under which a later whole-object use is rejected. Connect the static tree to runtime tombstones that prevent double destruction.

### 29. Liveness and ASAP Destruction

Describe backward liveness, last-use discovery, and insertion of `DropVar` instructions as soon as values become dead. Explain why Mojito follows observable ASAP destruction rather than merely dropping everything at lexical scope exit, and how copy/move/destructor methods turn lifetime placement into user-visible behavior.

### 30. Cleanup on Edges and Exceptional Flow

Cover drop order, edge-specific cleanup, loop exits, returns, try-region cleanup sets, handlers, and `finally`. Show why cleanup cannot always be represented as an instruction in the originating block and how MIR records cleanup on non-normal edges without double-dropping values.

## Part VII — The Register VM and Runtime

### 31. VM Architecture and Execution Frames

Introduce the register VM's program registry, function index, frame variables, registers, block driver, and instruction dispatch. Trace one MIR function from entry through block transitions to return. Explain the distinction between registers as transient computed values and variable slots as addressable storage needed for mutation and write-back.

### 32. Runtime Values and Materialization

Describe `Value` representations for scalars, strings, lists, tuples, structs, SIMD values, pointers, iterators, errors, and tombstones. Explain cloning versus copying semantics, coercion at binding sites, type-name reconstruction, and where the runtime intentionally trusts guarantees established by the checker.

### 33. Calling Functions in the VM

Walk through signature metadata, positional/keyword/default/variadic binding, frame construction, value-parameter reification, recursion, returns, and `mut`/`ref` write-back. Explain why some metadata is still rebuilt from declarations and identify the architectural direction toward carrying checked declaration metadata forward.

### 34. Methods, Constructors, and Lifecycle Dispatch

Explain struct construction through fieldwise initialization or `__init__`, method-symbol lookup, receiver insertion, overloaded method targets, `mut self` write-back, and ordinary mutable parameters. Include `__copyinit__`, `__moveinit__`, and `__del__` as examples where source syntax, canonical symbols, checked capabilities, and runtime behavior all intersect.

### 35. Builtins and Protocol Dispatch

Describe the boundary between VM instructions and helpers in `runtime/mod.rs`. Cover arithmetic, strings, lists, tuples, ranges, hashing, conversion, printing, and protocol-shaped dunder calls. Explain when the VM uses an intrinsic, when it dispatches to user code, and why synthetic protocol calls sometimes require an arity fallback rather than a checker-recorded source span.

### 36. Iteration and Collections at Runtime

Trace built-in and user-defined iteration through `GetIter`, `HasNext`, and `Next`, including the `Iterable`/`Iterator` type contracts. Cover list value semantics, in-place mutation through places, tuples, membership, indexing, and the interaction between runtime collection behavior and self-hosted collection implementations.

### 37. Exceptions, Returns, and Non-Normal Control Flow

Explain the VM's `Flow` model for normal completion, returns, jumps, and raised errors. Walk through execution of MIR try regions, cleanup, handlers, `else`, and `finally`, including the rule that control flow or an error from `finally` supersedes a pending outcome. Relate this runtime mechanism back to HIR region lowering and MIR cleanup annotations.

### 38. Unsafe Pointers and the Heap Model

Describe the arena-style heap used for `UnsafePointer`, allocation, offsets, loads, stores, aliasing, and the current no-reclamation simplification. Explain why pointer operations are useful for self-hosted data-structure experiments while remaining deliberately narrower than Mojo's full memory and origin model.

## Part VIII — Self-Hosting and Evolution

### 39. The Standard Library as a Compiler Test

Explain the self-hosting strategy: implement `Optional`, `List`, `Set`, `Dict`, `HashSet`, iterators, hashing, and math helpers in Mojito-flavored Mojo, then let honest library code expose missing compiler capabilities. Show how list-backed reference implementations serve as behavioral oracles for more advanced structures.

### 40. Case Study: Growing Traits from Library Pressure

Use the roadmap's progression from `Iterable` and `Comparable` through `Sized`, `Hashable`, numeric operation traits, and lifecycle markers to show demand-driven language design. Explain how each self-hosted algorithm produced a narrowly scoped checker/runtime feature and both positive and negative tests, avoiding speculative implementation of entire Mojo protocol families.

### 41. Case Study: Compile-Time Specialization

Develop a detailed example involving value parameters, associated comptime facts, and a self-hosted algorithm selected at compile time. Trace it through elaboration, checking, type identity, symbol construction, MIR, and runtime reification. Use the remaining nested-generic CTFE limitations to explain where specialization becomes substantially harder.

### 42. Testing the Whole Compiler

Describe unit-style stage tests, exact-output VM tests, ownership tests, module tests, symbol invariants, self-host tests, and directory-based assets classified by expected failure stage. Explain how to choose the narrowest useful regression test while still adding an end-to-end fixture for cross-stage bugs.

### 43. Architectural Boundaries and Common Failure Modes

Collect the most important boundaries from `docs/architecture.md`: checker versus MIR analysis, HIR versus MIR, MIR versus VM, and runtime helpers versus backend orchestration. Discuss recurring bugs such as reconstructing checked meaning from AST, duplicated symbol logic, mismatched scoping slots, executing parse-only constructs, and allowing runtime fallback to mask missing semantic information.

### 44. Extending Mojito Safely

Provide a repeatable feature-development recipe: update grammar, parse and preserve intent, define type rules, choose the proper lowering stage, extend analyses, implement runtime behavior, and add positive/negative/end-to-end coverage. Include decision questions for whether a feature belongs in elaboration, checking, HIR, MIR, analysis, runtime, or VM.

### 45. Current Limits and Future Directions

Close with an honest inventory of partially implemented or deferred areas: richer method/generic argument binding, `**kwargs`, trait default bodies, deeper CTFE specialization, full module boundaries, reference origins, backend/layout marker traits, writer protocols, and more complete overload ranking. Frame these as consequences of current architectural choices and self-hosting priorities rather than as a loose feature wishlist.

## Appendices

### Appendix A. Source-to-Stage Reference

Provide a compact table mapping language constructs to their token, AST, checker, HIR, MIR, analysis, VM, and runtime implementations. This becomes the quick navigation aid readers use after finishing the narrative chapters.

### Appendix B. Core Data Structure Catalog

List the important Rust types with short definitions and ownership relationships: `Token`, `Stmt`, `Expr`, `Type`, `Ty`, `TyArg`, checker registries, `Cfg`, `HirInstr`, `MirProgram`, `MirFunction`, `MirInstr`, `MirPlace`, ownership states, `Value`, `Prog`, and frame structures. Keep this referential rather than duplicating the explanatory chapters.

### Appendix C. Supported Mojo Surface

Summarize implemented, parse-only, and unsupported syntax and semantics, derived from `grammar.md`, `CLAUDE.md`, and the fixture directories. Version this appendix with the book so readers know which compiler revision the claims describe.

### Appendix D. Building the Book's Examples

Document how the book repository pins or checks out a Mojito revision, runs code listings through the compiler, and verifies expected output. Prefer executable examples and commit-specific source links so the independently versioned book does not silently drift away from the implementation.

## Suggested writing order

The publication order above is not the most efficient writing order. Begin by adapting the already mature pipeline material into Chapters 2, 7–9, 20–24, 26–37, and 43. Next write the type-system core in Chapters 10–19, using focused examples and the roadmap to explain design motivation. Only then write the compact frontend chapters and the broader self-hosting case studies. Write Chapters 1, 44, and 45 last, once the vocabulary and emphasis of the rest of the book are stable.

The first usable milestone does not need all 45 chapters. A strong initial edition could contain Chapters 1–3, 4–5, 7–10, 13–18, 20–24, 26–35, 39, 42–43, plus the first two appendices. The remaining chapters can then deepen specific subsystems without forcing a major reorganization of the table of contents.
