# Mojito Feature Matrix

This is the authoritative summary of language support. Update it in the same
change that alters a feature's status. [`grammar.md`](../grammar.md) remains authoritative for
surface syntax; the tests remain authoritative for detailed behavior.
The machine-checkable comparison with pinned Mojo—including explicit divergences
and exclusions—lives in [`conformance/parity.tsv`](../conformance/parity.tsv).

Status meanings:

- **Run** — accepted by the production compiler and executed by the register VM.
- **Check** — represented and checked, but has no independent runtime behavior.
- **Parse** — preserved in the AST but deliberately rejected before execution.
- **No** — not reliably accepted even as syntax.

| Area | Feature | Status | Boundary / notes |
|---|---|---:|---|
| Frontend | Indentation, continuations, comments, numeric/string literals, Mojo escapes | Run | Includes current radix/decimal/exponent separators, `Byte == UInt8`, raw and triple forms, and same-line or indented adjacent strings. Diagnostic parsing can report multiple statement-level errors. |
| Frontend | T-strings | Run | Prefix order/case and raw/triple combinations are accepted; nested lexical structure is respected. Interpolations require `Writable`, convert through `String`, and concatenate in source order. Lazy self-hosting remains in Complete MIR-Schema-Prerequisite CPU Semantics. |
| Frontend | Walrus `:=` | Run | Evaluates once and introduces a function-scoped binding while producing the value. |
| Modules | File-scope declarations and runtime entry | Run | Runtime statements at file scope are rejected; execution begins in zero-argument `main()`. |
| Bindings | Typed/inferred `var`, late initialization, var-less introduction, assignment | Run | Reads require initialization on every reaching path; same-scope redeclaration and type changes are rejected. An implicit binding introduced in a nested block has function scope, while path joins retain a maybe-uninitialized state unless every reachable path assigns it. |
| Bindings | Field/index places, augmented assignment, tuple unpacking | Run | Place expressions evaluate indexes once. |
| Bindings | Local `ref name = place` | Run | Aliases variable/field/index storage; indexed places bind once; persistent CFG loans enforce exclusivity through last use. |
| Control flow | Loop `else`, `for ref`, context managers | Run | `break` bypasses loop `else`; list reference iteration writes through element handles; `with` executes checked enter/exit calls, including raising exits. |
| Calls | Defaults, keywords, `/`, `*`, homogeneous `*args` | Run | One structural contract serves free, method, static-method, and hand-written constructor calls in the checker and VM. |
| Calls | Homogeneous `**kwargs` and `**kwargs^` forwarding | Run | Free, generic, instance, static, and bounded-trait calls materialize an owned, insertion-ordered `StringDict[T]`. Forwarding consumes it through the shared binder, specialization, duplicate, origin, ownership, and effect checks. |
| Calls | Heterogeneous `*args: *ArgTypes` | Run | Per-element bound checking, type-erased runtime collection, and compile-time `args.__len__()` iteration/indexing run for literal and directly constructed arguments. Specialization exposes the concrete type of each statically indexed pack element. |
| Calls | Generic callable values | Run | Expected `def(...) -> ...` types contextually instantiate generic functions, including narrowly inferable generic indirect calls. |
| Calls | Method/generic argument-binding parity | Run | Includes ordinary-parameter write-back. |
| Calls | User-defined static methods | Run | Uses the same positional/default/keyword ABI without a receiver slot. |
| Calls | Callable expressions / first-class function values | Run | Non-capturing, generic, and contextually selected overloaded functions can be stored, passed, and indirectly invoked with checked effects. User structs are callable only when they nominally conform to a matching `def(...)` contract and implement `__call__`. |
| Calls | Named `out` results | Run | A free function's single `out` parameter is a caller-transparent result slot and must be initialized before fallthrough or bare return. |
| Conventions | `imm`/legacy `read`, `var`, `mut`, `ref` parameters and `ref self` | Run | `imm` and its temporary compatibility synonym `read` share immutable-reference semantics. Copyable immutable arguments overlapping a `mut` argument are materialized; non-Copyable aliases remain errors. |
| Conventions | `out self`, named `out` results, `deinit self` | Run | Lifecycle receivers and a single free-function named result are supported. |
| Lifecycle | `ImplicitlyDeletable`, unified copy/move initialization, and `@implicit` constructors | Run | Ordinary values are implicitly deletable; an explicitly false conditional conformance makes a type linear. Legacy unified-initializer spellings remain documented compatibility extensions. |
| Lifecycle | `@explicit_destroy` | Run | The required string argument customizes linearity diagnostics but does not itself make a type linear. Named `deinit self` methods discharge path-sensitive obligations. Abandonment, overwrite, partial/double/conditional destruction are rejected; raising destruction preserves the value for an `except` fallback and automatic `DropVar` destruction is suppressed. |
| Functions | Recursion and conservative return checking | Run | Sibling forward references and mutual recursion are rejected. |
| Functions | Non-escaping unified closures | Run | Explicit `imm`/legacy `read`, `mut`, moved, and default capture conventions lower to closure environments. Sibling forwarding, recursion, nested generics, closure values, and reference-backed mutation run; escape is rejected. |
| Functions | Escaping closures | Excluded | Current Mojo does not support closures that outlive their enclosing scope; this is not a parity target. |
| Types | Scalars, strings, lists, tuples, ranges, SIMD and slices | Run | Slice syntax preserves omitted, negative, and strided bounds; selects `ContiguousSlice` or `StridedSlice`; exposes optional bounds and `indices()`; and supports checked mixed/variadic user `__getitem__` and `__setitem__`. Built-in List/String result APIs remain CPU semantic/library work. Tuples include bare displays/destructuring, inferred or typed construction, compile-time indexing, length, membership, comparison, concatenation, and reversal. `Int` and `Scalar[DType.int]` are one checked type; SIMD is value-level, not machine-vector code generation. |
| Types | Collection displays and comprehensions | Run | Homogeneous list, set, and dictionary displays plus nested/filtering CPU comprehensions infer their element types, preserve key-before-value and clause evaluation order, enforce hashing/ownership requirements, and lower to explicit collection-construction MIR. General user-protocol construction remains schema-prerequisite work. |
| Types | `std.utils.Variant` tagged unions | Run | Explicit type packs, construction, `isa[T]()`, compile-time `is_type_supported[T]()`, projection, `set[T](value)`, consuming `take[T]()`/`unsafe_take[T]()`, and ownership-returning `replace[Tin, Tout](value)`/`unsafe_replace` use explicit checked MIR. Unsupported arms reject statically, checked operations validate tags, and lifecycle/Hashable/Writable/Equatable availability is gated element-wise. `destroy_with`, representation writing, and full TypeList-driven library APIs remain standard-library work. |
| Types | Origin-carrying parameter reference types | Check | Named/place origins, semantic-only `Origin` parameters, and fixed/symbolic mutability declarations are checked. |
| Types | Origin-carrying return reference types | Run | Parameter/receiver projections, unions, call substitution, invalid-local escape rejection, and returned alias execution are implemented. |
| Structs | Fields, fieldwise/manual construction, methods, copy/move/drop | Run | Structs have value semantics. |
| Generics | Typed parameters, defaults, constraints, and packs | Run | Type and typed scalar/aggregate value parameters support positional/named binding, inference, dependent defaults, infer-only declarations, trailing `where` predicates, type equality, and heterogeneous compile-time packs including `conforms_to(Ts.values, Trait)`; origin parameters are semantic facts. |
| Metaprogramming | Unified `reflect[T]` and declaration selection | Check | Struct facts, named field indexes/types, dependent reflected types, and declaration-producing `comptime` branches elaborate before checking. Current `.field[name]` and `.field_at[index]` return chainable handles whose selected type is `.T`; the removed `field_type` spelling is rejected. ABI byte offsets belong to native backends. |
| Traits | Requirements, nominal conformance, associated comptime facts | Run | Includes the protocols exercised by the self-hosted library. |
| Traits | Refinement and default method bodies | Run | Requirements/capabilities inherit; defaults are statically materialized, explicit methods override, and ambiguity is rejected. |
| Traits | Associated-type composition and conditional conformance | Run | Associated bounds merge across refinements; per-conformance `where` predicates are evaluated after type/value specialization. |
| Protocols | Indexer, Hasher, Writer, and Writable | Run | User indexes normalize through `__mlir_index__`; hashing is incremental and caller-provided; display/repr and String fields stream through Writer with reflective defaults. |
| Overloading | Functions, methods, constructors | Run | Ranking minimizes conversions after applying the contextual `Int`/`Float64` defaults to unconstrained literals, then prefers fixed arity, shorter signatures, and concrete ties. Checked `@implicit` constructors participate and genuinely equivalent conversions remain ambiguous. |
| Comptime | Constants, `comptime if`, `comptime for`, type facts | Run | Elaborated before checking; literal values materialize while type/reflection handles erase. |
| Comptime | Unified `reflect[T]` queries | Run | Struct detection, field collections, named field indexes, chainable named/by-index field handles, dependent `.T` types, and declaration-producing compile-time selection run; ABI offsets are native-backend facts and field references use checked projections. |
| Comptime | Pure top-level CTFE through MIR/VM | Run | Fuel bounded; reflected compile-time branches may select and unroll declarations before name collection, while arbitrary string-to-AST generation remains unsupported. |
| Control flow | `if`, `while`, `for`, `break`, `continue`, ternary | Run | User iterator dispatch, loop `else`, and `for ref` write-through run. `for var item in collection^` consumes the source, moves non-Copyable elements, drops permitted residual elements on early exit, and rejects early abandonment of linear residuals. |
| Exceptions | `raise`, `try`/`except`/`else`/`finally` | Run | Direct, selected overload/method, indirect callable, trait-requirement, and bounded-dispatch effects are checked with exact typed error contracts; inferred `except` bindings and `Never` run. |
| Contexts | `with` statements | Run | Checked `__enter__`/`__exit__` calls execute around the body, including raising exits and manager mutation/write-back. |
| Modules | Source modules/packages, wildcard/selective, dotted/relative linking | Run | Every dotted prefix binds a namespace; ordinary directories can form namespace paths; package members require explicit import or `__init__.mojo` re-export; and a source package wins over a same-named source module. Compiled `.mojoc`/`.mojopkg` lookup remains Packaging, Artifacts, And Developer Tooling work. |
| Modules | Qualified `import module`, module/member aliases | Run | Aliased and full dotted calls, values, and types resolve without merging same-named declarations. |
| Ownership | `^` moves, partial moves, use-after-move analysis | Run | MIR dataflow owns these rules. |
| Borrowing | Local loans and origin-bearing cross-call references | Run | Frame/slot handles execute reborrows, free and receiver-projected returns, unions, captured indexes, and reference-valued aggregates. Static, untracked, and unsafe-any contracts lower explicitly; executable origin-bearing pointer loans remain open. |
| Unsafe pointers | Provenance, arithmetic, and lifetime | Run | Stable allocation identities, element offsets, allocation bounds, aligned allocation, dangling placeholders, real deallocation, and invalid/double-free diagnostics execute in the VM. |
| Destruction | ASAP `__del__`, edge/try cleanup, reverse field order | Run | Liveness rewrites MIR with explicit drops. |
| Standard library | Self-hosted collections, algorithms, math, hashing | Run | Proof subset under `stdlib/`, not Mojo's full standard library. |
| Backend | Register VM | Run | Sole backend and runtime; direct calls use an explicit continuation-driven frame stack with monotonic frame identities. |
| Tooling | Textual MIR/VM assembly, parser, verifier, disassembler | No | Planned as a versioned Mojito-owned serialization and debugging format. |
| Backend | Cranelift, then LLVM | No | Planned native backends after the textual MIR contract and VM semantics stabilize. |
| Stretch backend | eBPF and MLIR | No | Explicit stretch goals, not first-pass parity requirements. |
| Out of scope | GPU, concurrency/parallelism, distributed execution, Python interop | No | Intentionally excluded from the first Mojito parity target. |

For planned semantic work, see [`roadmap.md`](../roadmap.md). For exact VM
operations, see [`vm-instruction-set.md`](vm-instruction-set.md).
