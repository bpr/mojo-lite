# Mojo Nightly Target

Mojito tracks **Mojo 1.0.0b3.dev2026071705 (2026-07-17)** as its language
comparison target. The version is taken from the official cumulative nightly
release page:

- <https://mojolang.org/releases/nightly/>

The differential runner must still report the actual locally installed
`mojo --version`; updating this document or the parity-manifest header does not
claim that every nightly change has already been implemented.

## Review Policy

Before closing a language-parity milestone:

1. Compare the target above with the version at the top of the nightly page.
2. Review every intervening **Language enhancements** and **Language changes**
   entry, not only the stable manual.
3. Update `conformance/parity.tsv` first. A newly introduced mismatch becomes a
   documented subset/divergence until implementation and differential evidence
   justify `implemented`/`match`.
4. Update `roadmap.md`, grammar and architecture documentation, fixtures, and
   bundled Mojo sources affected by removed or renamed syntax.
5. Run differential conformance with a Pixi environment containing the exact
   target build and retain the reported version with the results.

## 1.0.0b3 Nightly Drift From Mojito's Previous Baseline

The following CPU-language changes affect Mojito directly.

| Area | Current nightly | Mojito consequence |
|---|---|---|
| Immutable convention | `imm` is the preferred spelling for the argument and closure-capture convention. `read` remains a synonym but is headed for deprecation. | Accept and emit `imm`; retain `read` only as a compatibility spelling. The linked commit `323dfd974e2f6fc83ce82a476d8fa5d51529eadf` documents this transition. |
| Linear-value trait | `ImplicitlyDestructible` was renamed to `ImplicitlyDeletable`; `is_trivially_destructible` likewise became `is_trivially_deletable`. | Reverse Mojito's previous vocabulary migration and update constraints, diagnostics, tests, and bundled sources. |
| Explicit destruction | `@explicit_destroy` no longer opts a type out of implicit deletion. A type narrows or removes `ImplicitlyDeletable` through conditional conformance, commonly `ImplicitlyDeletable where False`. The decorator is optional and only supplies an explanatory diagnostic; using it without a message is an error. | Separate the linearity fact from the diagnostic decorator and derive automatic deletion from conformance. |
| Constraints | Parameter-list `where` clauses were removed. Only trailing declaration `where` clauses remain. Type equality now uses `==`/`!=`; `_type_is_eq` was removed. Pack operands such as `Ts.values` work with `conforms_to`. | Reject the formerly accepted parameter-list form, expand the checked predicate algebra, and update fixtures. |
| Closures and callables | Unified closures use explicit captures with `imm`, `mut`, moves (`x^`), and optional default conventions. Dynamic function pointers can retain narrowly inferable unbound parameters. User structs must explicitly conform to a `def(...)` closure trait; compatible `__call__` alone is no longer enough. | Complete environment-bearing closure values, generic indirect calls, capture origins, and nominal callable-trait conformance. |
| Reflection | `Reflected.field_type[name]` became `Reflected.field[name]`; the result is a chainable reflected handle whose type is `.T`. `field_at[index]` is the by-index counterpart. | Implemented with current `reflect[T]` syntax, nested handle chaining, named/indexed diagnostics, and rejection of `field_type`. |
| Integer/SIMD model | `Int` is now an alias for `Scalar[DType.int]`. SIMD-width inference uses the new `SIMDSize` parameter type, or `_` for an unbound width. | Revisit Mojito's distinct `Ty::Int` representation and width-parameter classification before claiming scalar/SIMD parity. |
| Origins and pointers | Struct fields may not hide `UnsafeAnyOrigin`; use an explicit origin parameter or `UntrackedOrigin`. Implicit widening conversions to unsafe-any origins are deprecated or removed, and pointer optionals preserve concrete origins. | Hidden unsafe-any fields are rejected and there is no implicit unsafe-origin widening. `UnsafePointer(to=place)` infers a concrete place origin with executable owner loans; a place pointer coerces only to a declared origin parameter at aggregate-storage sites. |
| Imports and artifacts | Resolution order is source package, `.mojoc`, source module, then legacy `.mojopkg`. Relative imports require `from`; dotted absolute imports bind every prefix; intra-package implicit visibility is deprecated. | Source precedence, prefix namespaces, and explicit intra-package visibility are implemented. `.mojoc`/`.mojopkg` loading remains in Packaging, Artifacts, And Developer Tooling. |
| Keyword variadics | `**kwargs` may be forwarded with `**kwargs^`; the standard owning container is now `StringDict`. | Homogeneous free, generic, instance, static, and bounded-trait collectors use owned `StringDict[T]` values; consuming forwarding runs through the shared binder and its specialization, ownership, origin, duplicate, and effect checks. |
| Owned iteration | `for var x in collection^` supports moving non-Copyable elements; collection deletion conformance is conditional on element capabilities. | Consuming collection iteration moves the source and each element, destroys implicitly deletable residual state on early exit, and rejects early exit when linear residual elements would be abandoned. |

Python/NumPy additions, GPU changes, and distributed/concurrent facilities remain
outside Mojito's declared first-pass scope.
