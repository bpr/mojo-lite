# Changelog

All notable changes to Mojito will be documented in this file. The project uses
Semantic Versioning while its public Rust API and supported Mojo subset continue
to evolve under the `0.x` compatibility rules.

## [Unreleased]

### Added

- `UnsafePointer(to=place)` infers an origin-bearing pointer whose provenance is
  the concrete source place, with mutability taken from the owner binding. The
  checked pointer type retains the origin through HIR and MIR; the VM represents
  the value as an origin-free frame/slot handle. Pointer bindings and
  pointer-storing aggregates carry executable owner loans: the owner stays alive
  through the pointer's last use, and overlapping access, owner invalidation,
  and dangling escapes (`PointerEscapesOrigin`) are rejected statically. A place
  pointer binds a declared field origin parameter at aggregate-storage sites
  without inventing mutable capability, and non-zero offsets, arithmetic,
  comparison, and `free()` on origin-bearing pointers are rejected as a strict
  subset.

- Source imports now follow the current source-side namespace rules: source
  packages beat same-named source modules, ordinary directories can form dotted
  namespace paths, every dotted prefix binds, and submodules require explicit
  import or package-initializer re-export. Compiled `.mojoc`/`.mojopkg` lookup is
  reserved for the versioned artifact work.

- Homogeneous `**kwargs` collectors now use the self-hosted, insertion-ordered
  `StringDict[T]`. A final `**kwargs^` consumes and forwards its entries through
  the shared call binder with duplicate and element-type checking.

- Slice syntax now distinguishes `ContiguousSlice` and `StridedSlice`, preserves
  optional/negative bounds, implements `indices(length)`, and dispatches checked
  mixed or variadic `__getitem__` and `__setitem__` arguments, including slice
  assignment. Built-in collection view/API parity remains standard-library work.

- `std.utils.Variant` now supports compile-time type-membership queries,
  checked and unchecked consuming extraction, and checked and unchecked
  ownership-returning replacement. Unsupported arms reject statically, checked
  operations validate runtime tags, and `take` participates in use-after-move
  analysis.

- Current Mojo literal spellings now include leading/trailing-point floats,
  exponent forms, repeated/trailing digit separators, raw and case-insensitive
  string prefixes, one-to-three-digit octal escapes, triple-string line
  suppression, adjacent ordinary and t-string forms, nested interpolation
  boundaries, and the `Byte == UInt8` alias. Mojo does not define a distinct
  byte-string literal family.

- `CheckedProgram` now exposes stable checked expression and declaration arenas
  with child identities, resolved types, value/place/type categories, binding
  owners, extensible effect facts, and explicit semantic adjustments. Call,
  conversion, move, and explicit-destruction decisions are canonical node data.
  VM CTFE now passes rewritten fragments through the authoritative checker, and
  MIR retains checked types for source-derived registers.

- Checked HIR now retains stable checked-node identity, resolved type, value
  category, and semantic adjustments through function and exception-region CFGs.
  MIR consumes checked call/conversion/destruction decisions directly. Stored
  origin-parametric reference fields preserve frame/slot handles and owner loans,
  and user-defined slicing dispatches a checked `Slice` through `__getitem__`.

- Checked HIR and MIR places now retain root, per-projection, and final storage
  types. Production lowering verifies complete typed-place metadata before VM
  execution, and reference field reads/writes use the checked storage type rather
  than rediscovering reference semantics from runtime values.

- Unsafe pointers now retain allocation provenance and typed offsets, support
  arithmetic, same-allocation subtraction, equality, aligned allocation and
  non-null dangling placeholders, and diagnose out-of-bounds access, invalid
  frees, double frees, and use after free. Static, untracked, and unsafe-any
  reference origins now lower into checked contracts, and local reborrows retain
  executable reference handles.

- CPU-language surface work now includes definite late initialization,
  function-scoped implicit and walrus bindings, context-manager elaboration,
  loop `else`, list `for ref`, declaration destructuring, Writable-backed
  t-strings, integer bitwise/shift operators, and `__matmul__` dispatch.

- Callable and closure semantics now include contextually selected overloaded
  function values with effects, generic callable specialization, explicit
  unified capture conventions, sibling and generic nested calls, reference-backed
  closure environments, and nominal `def(...)` callable structs. Escaping
  closures remain statically rejected.

- A versioned Mojo nightly audit now tracks 1.0.0b3.dev2026071705 and records
  breaking drift affecting immutable conventions, linear deletion, constraints,
  closures, reflection, scalar/SIMD types, origins, imports, and keyword
  variadics.

- Compile-time parameters support typed scalar and aggregate values, type/value
  defaults, named arguments, infer-only parameters, dependent defaults and
  predicates, and heterogeneous type/value packs with per-index types.
- Generic constraints cover parameter and trailing `where` clauses, boolean and
  comparison predicates, `conforms_to`, conditional methods, and conditional
  conformance.
- Specialization uses structural cache keys, a deduplicated shared-fuel worklist,
  and source-located quota diagnostics.
- Current `reflect[T]` handles expose compile-time struct detection, field
  counts, names, types, named field indexes, and chainable `.field[name]` /
  `.field_at[index]` reflected handles whose selected type is `.T`. The removed
  `field_type` spelling is rejected, and reflection can drive
  declaration-producing compile-time branches.
- Generic-target `@implicit` conversions substitute concrete target parameters
  before constructor matching.

- Trait associated-type requirements compose bounds across refinements, and
  conditional conformance predicates are evaluated after type/value specialization.
- Current Indexer normalization, incremental caller-provided hashing, UTF-8 Writer
  buffering, Writable display/repr hooks, reflective formatting defaults, and
  String replacement fields replace the former direct `__str__` formatter path.

- Current Mojo consuming parameters use `var`; the removed `owned` spelling is
  rejected, and the convention is represented as `Var` throughout the compiler.
- Unified `__init__(out self, *, copy: Self)` and
  `__init__(out self, *, move: Self)` lifecycle declarations drive copy and move
  construction through the existing checked MIR and VM lifecycle machinery.
- Calls materialize Copyable `imm` arguments before overlapping `mut`/`ref`
  access, allowing calls such as `f(mut x, x)` while retaining alias errors for
  non-Copyable values and multiple exclusive accesses.
- Current `ImplicitlyDeletable` lifecycle vocabulary replaces the superseded
  `ImplicitlyDestructible` spelling in bundled sources and generic checking.
- Validated, nonraising `@implicit` constructors now provide explicit MIR-lowered
  conversions for typed bindings, arguments, returns, and overload selection.
- `ImplicitlyDeletable where False`, rather than `@explicit_destroy`, now makes
  a type linear. The decorator requires a string and only supplies its
  diagnostic. Field-sensitive obligations preserve partial moves and projected
  destruction while rejecting whole destruction of incomplete aggregates,
  double and conditional destruction; raising destructors preserve the value
  for an `except` fallback, and automatic VM destruction is suppressed.
- Generic constraints now use only trailing `where`, compare types with
  `==`/`!=`, and accept pack-wide `conforms_to(Ts.values, Trait)`. `Int` is the
  canonical VM representation of `Scalar[DType.int]`; `SIMDSize` width values
  and `_` construction-width inference follow the pinned nightly vocabulary.

## [0.1.0] - 2026-07-15

Initial crates.io release.

### Added

- Indentation-sensitive lexer, Pratt parser, semantic checker, HIR and flattened
  MIR pipeline, ownership analysis, drop elaboration, and register VM.
- Functions, methods, structs, traits, generics, overloads, compile-time
  elaboration and VM-backed CTFE for the supported subset.
- Move checking, partial moves, ASAP destruction, stable origins, persistent
  loans, local and cross-call references, reference returns, and frame/slot
  runtime handles.
- Scalar, string, list, tuple, range, exception, iterator, unsafe-pointer, and
  VM-emulated `SIMD[...]` lane-vector semantics needed by the bundled self-hosted
  standard-library proofs. The VM executes lanes serially; hardware SIMD and
  native vector code generation are not included.
- Dotted, relative, qualified, and aliased source-module imports; package
  `__init__.mojo` discovery and re-exports; collision-free linked identities;
  and bundled `std` search roots.
- CLI stages for lexing, parsing, checking, ownership verification, and running
  `.mojo` source files.
- A versioned CPU-parity manifest and Pixi-driven differential harness for
  matching execution output and matching compiler rejection against a pinned
  Mojo reference build.
- A validated Mojo 1.0.0b2 manual inventory that distinguishes parity,
  strict-subset gaps, divergences, representation differences, exclusions, and
  stretch goals; every recorded divergence has an executable differential case.
- An expanded differential corpus covering the implemented first-pass parity
  surface with matching execution, matching rejection, strict-subset,
  acceptance-divergence, and output-divergence modes. The comparison also pins
  lowercase Bool formatting and Mojito's conservative same-place mutable-call
  rejection as known differences from the reference build.
- Mojo-compatible module-scope validation: production compilation rejects
  executable file-scope statements and enters runtime code through `main()`.
- Source package namespace completion includes wildcard privacy for
  underscore-prefixed declarations and isolates same-named declarations and
  overload sets from different modules.
- Module namespaces now preserve lexical shadowing, support imports inside
  functions and nested blocks, resolve unaliased full dotted paths and exported
  types, and implement dots-only relative sibling-module imports.
- User-defined static methods now type-check, participate in overload selection,
  lower without an implicit receiver, and execute with default and keyword arguments.
- `raise` now requires a surrounding handler or a `raises` function/method, and
  direct calls to raising free functions must be handled or propagated.
- Raising instance and static methods now retain their effect through method
  overload selection, so calls must likewise be handled or propagated.
- Non-capturing functions are runtime values with checked function types and can
  be stored, passed as arguments, and invoked through MIR indirect calls.
- Function types retain their `raises` effect; selected free-function overloads
  and indirect callable calls now require effect handling or propagation.
- Typed and parametric errors now survive parsing and checking through direct,
  overloaded, method, and indirect calls. Handlers receive the inferred typed
  error value, and `Never` acts as the bottom and nonraising error type.
- Free functions support a single named `out` result with caller-transparent
  invocation, checked initialization, and direct VM return-slot execution.
- Generic free functions accept heterogeneous `*args: *ArgTypes` packs, check
  every supplied type against the pack bound, and execute type-erased pack
  length queries. Compile-time loops can specialize literal/constructed packs,
  query `args.__len__()`, and index elements through their common bound.
- Expected function types contextually specialize non-overloaded generic function
  values for checked indirect invocation. Hand-written constructors now share
  default and keyword argument binding with free and method calls.
- Overload selection now follows first-pass Mojo precedence across conversion
  counts, fixed versus variadic candidates, signature length, and generic ties;
  defaulted and variadic declarations can participate in overload sets while
  overlapping defaulted calls retain ambiguity.
- Trait refinement now inherits method and associated-member requirements, and
  executable defaults are statically materialized with override/ambiguity rules.
- Lifecycle definite initialization follows normal, returning, raising,
  branching, looping, and protected exceptional paths instead of collecting
  assignments flow-insensitively.
- Opaque trait-bounded indexing dispatches through `__getitem__`; the self-hosted
  library includes an incremental hasher proof; and user-defined printed values
  must opt into Writable/Representable formatting. Bool output is `True`/`False`.

### Scope

- Targets an evolving single-threaded CPU subset of Mojo.
- GPU execution, concurrency/parallelism, distributed execution, Python
  interoperability, MLIR, and optimized native code generation are not included.

[0.1.0]: https://github.com/bpr/mojito/releases/tag/v0.1.0
