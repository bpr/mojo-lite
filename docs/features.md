# Mojito Feature Matrix

This is the authoritative summary of language support. Update it in the same
change that alters a feature's status. [`grammar.md`](../grammar.md) remains authoritative for
surface syntax; the tests remain authoritative for detailed behavior.

Status meanings:

- **Run** — accepted by the production compiler and executed by the register VM.
- **Check** — represented and checked, but has no independent runtime behavior.
- **Parse** — preserved in the AST but deliberately rejected before execution.
- **No** — not reliably accepted even as syntax.

| Area | Feature | Status | Boundary / notes |
|---|---|---:|---|
| Frontend | Indentation, continuations, comments, literals, Mojo escapes | Run | Diagnostic parsing can report multiple statement-level errors. |
| Frontend | T-strings | Parse | Interpolations are parsed; semantic lowering is unsupported. |
| Frontend | Walrus `:=` | Parse | Typed as its value, then rejected by MIR execution. |
| Bindings | Typed/inferred `var`, var-less introduction, assignment | Run | Same-scope redeclaration and type changes are rejected. |
| Bindings | Field/index places, augmented assignment, tuple unpacking | Run | Place expressions evaluate indexes once. |
| Bindings | Local `ref name = place` | Run | Aliases variable/field/index storage; indexed places bind once; persistent CFG loans enforce exclusivity through last use. |
| Calls | Defaults, keywords, `/`, `*`, homogeneous `*args` | Run | One structural contract in `src/call.rs` serves checker and VM. |
| Calls | Homogeneous free-function `**kwargs` | Run | Materialized as self-hosted `HashDict[String, T]`. |
| Calls | Method/generic argument-binding parity | Run | Includes ordinary-parameter write-back. |
| Calls | Callable expressions / first-class function values | Parse | Bare-name calls and supported nested defs run; general `Invoke` does not. |
| Conventions | `read`, `owned`, `mut`, `ref` parameters and `ref self` | Run | Origin-checked reference paths use frame/slot handles; ordinary non-reference `mut` paths may use value write-back. |
| Conventions | `out self`, `deinit self` | Run | Constructor/destructor receivers only. Ordinary `out` is unsupported. |
| Functions | Recursion and conservative return checking | Run | Sibling forward references and mutual recursion are rejected. |
| Functions | Non-escaping nested `def` captures | Run | Lifted downward funargs; generic/deep/sibling-calling forms are rejected. |
| Functions | Escaping closures and general function values | Parse | Representable syntax is rejected; not a Mojito runtime capability. |
| Types | Scalars, strings, lists, tuples, ranges, SIMD and slices | Run | SIMD is value-level, not machine-vector code generation. |
| Types | Function type annotations | Parse | Represented as `Type::Func`; binding semantics are unsupported. |
| Types | Origin-carrying parameter reference types | Check | Named/place origins, semantic-only `Origin` parameters, and fixed/symbolic mutability declarations are checked. |
| Types | Origin-carrying return reference types | Run | Parameter/receiver projections, unions, call substitution, invalid-local escape rejection, and returned alias execution are implemented. |
| Structs | Fields, fieldwise/manual construction, methods, copy/move/drop | Run | Structs have value semantics. |
| Generics | Bounded type parameters and `Int`/`Bool` value parameters | Run | Type parameters erase; value parameters participate in identity; origin parameters are compile-time semantic facts. |
| Traits | Requirements, nominal conformance, associated comptime facts | Run | Includes the protocols exercised by the self-hosted library. |
| Traits | Refinement and default method bodies | Parse | Rejected by the checker. |
| Overloading | Functions, methods, constructors | Run | Static conservative ranking; canonical symbols come from `symbol.rs`. |
| Comptime | Constants, `comptime if`, `comptime for`, type facts | Run | Elaborated before checking. |
| Comptime | Pure top-level CTFE through MIR/VM | Run | Fuel bounded; generated declarations remain unsupported. |
| Control flow | `if`, `while`, `for`, `break`, `continue`, ternary | Run | User iterator protocol is supported. |
| Exceptions | `raise`, `try`/`except`/`else`/`finally` | Run | `raises` syntax is recorded; effect checking is not enforced. |
| Contexts | `with` statements | Parse | Context-manager protocol is unsupported. |
| Modules | `from` imports, wildcard/selective, dotted/relative linking | Run | Dependency declarations are flattened with module provenance. |
| Modules | Plain `import module` and aliases | Parse | No qualified module namespace lookup yet. |
| Ownership | `^` moves, partial moves, use-after-move analysis | Run | MIR dataflow owns these rules. |
| Borrowing | Local loans and origin-bearing cross-call references | Run | Frame/slot handles execute free and receiver-projected returns, unions, and captured indexes; unsafe/static origin forms remain deferred. |
| Destruction | ASAP `__del__`, edge/try cleanup, reverse field order | Run | Liveness rewrites MIR with explicit drops. |
| Standard library | Self-hosted collections, algorithms, math, hashing | Run | Proof subset under `stdlib/`, not Mojo's full standard library. |
| Backend | Register VM | Run | Sole backend and runtime; direct calls use an explicit continuation-driven frame stack with monotonic frame identities. |
| Tooling | Textual MIR/VM assembly, parser, verifier, disassembler | No | Planned as a versioned Mojito-owned serialization and debugging format. |
| Backend | Cranelift, then LLVM | No | Planned native backends after the textual MIR contract and VM semantics stabilize. |
| Stretch backend | eBPF and MLIR | No | Explicit stretch goals, not first-pass parity requirements. |
| Out of scope | GPU, concurrency/parallelism, distributed execution, Python interop | No | Intentionally excluded from the first Mojito parity target. |

For planned semantic work, see [`roadmap.md`](../roadmap.md). For exact VM
operations, see [`vm-instruction-set.md`](vm-instruction-set.md).
