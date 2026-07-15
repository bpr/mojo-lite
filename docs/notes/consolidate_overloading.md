# Change description: consolidate overload signature / lowered-name logic

Reviewer notes for the Phase 5b cleanup task ("Consolidate signature identity
and lowered-name construction behind one canonical symbol API", formerly
roadmap Phase 5b).

> **Scope caveat:** the working tree contains earlier uncommitted work in other
> files (`src/error.rs`, `src/hir/mod.rs`, parts of `tests/checker_test.rs`,
> `tests/lexer_test.rs`, `tests/module_test.rs`, and doc edits predating this
> task). This document covers only the overload-consolidation change set listed
> below. `cargo fmt` was run repo-wide as part of the `scripts/check` gate, so
> a few mechanical formatting diffs may appear outside the listed regions.

## Files in this change set

| File | Kind of change |
|---|---|
| `src/symbol.rs` | **new** — the canonical symbol module (~320 lines) |
| `src/lib.rs` | +1 line: `pub mod symbol;` |
| `src/checker.rs` | replaced its private mangling helpers with wrappers over `symbol` |
| `src/mir/mod.rs` | routed lowering through `symbol`; deleted duplicated helpers |
| `src/backend/vm.rs` | routed registries/dispatch through `symbol`; deleted duplicated helpers |
| `tests/symbol_test.rs` | **new** — 8 focused symbol tests incl. a source-tree scan |
| `assets/ok/overloading_struct_params.mojo` | **new** — end-to-end fixture for the fixed drift |
| `roadmap.md`, `docs/architecture.md` | status/doc updates |

## 1. Background: the state before this change

Overloaded declarations lower to signature-qualified names (`pick$ov$Int`,
`Box.__init__$ov$String`). Before this change, three copies of the string
logic existed:

- **`src/checker.rs`** (~lines 116–177): `callable_lowered_name`,
  `method_lowered_name`, `callable_signature_suffix`,
  `method_signature_suffix`, `signature_suffix`, and a `type_mangle` over the
  checker's resolved `Ty`. Used to record the exact resolved callee per call
  span (`overload_targets: HashMap<Span, String>`), which MIR embeds into
  `Call`/`MethodCall` instructions.
- **`src/mir/mod.rs`** (~lines 2105–2225): `top_level_overloads`,
  `method_overloads`, `overloaded_def_name`, `overloaded_method_name`,
  `signature_suffix`, and a **second** `type_mangle` over the declared
  `ast::Type`, plus `value_arg_mangle`. Used to *name* each emitted
  `MirFunction`. Also duplicated `lifecycle_method_name` /
  `is_mojo_copy_constructor` (the `__init__(out self, *, copy: Self)` →
  `__copyinit__` rename).
- **`src/backend/vm.rs`** (~lines 1675–1818): a **verbatim third copy** of all
  seven MIR helpers, used by `build_sigs` (calling-signature registry keys) and
  `build_structs` (the `mut_self_methods` set). Plus `Prog::overload_name`
  hand-assembling the `format!("{name}$ov$")` prefix for arity fallback, and
  `call_named` reparsing constructor targets with
  `name.split_once(".__init__$ov$")`.

## 2. The latent bug this exposed (and fixes)

The checker-side and MIR/VM-side manglings **had already drifted** for
non-scalar parameter types:

| Declared parameter type | checker `type_mangle(Ty)` (old) | MIR/VM `type_mangle(Type)` (old) |
|---|---|---|
| `Point` (a struct) | `Struct$Point` | `Point` |
| `Pair[Int]` | `Struct$Pair$Int` | `Pair$Int` |
| `T` (a type parameter) | `Param$T` | `T` |
| `Self.T` | `Param$T` | `SelfParam$T` |
| `UnsafePointer[T]` | display fallback (`UnsafePointer$T$` — trailing `$` from `]`) | `UnsafePointer$T` |

Consequence: for any overload set containing such a parameter, the checker
recorded a callee (e.g. `pick$ov$Struct$Point`) that **named no emitted MIR
function**. For free-function calls the MIR embeds that exact name in
`FuncRef`, the VM's `call_named` fails `index_of`, and the program dies with
`"vm backend does not support the built-in or callee 'pick$ov$Struct$Point'
yet"`. For method calls the embedded `resolved` name likewise misses and the
call errors. Reproduced before the change with:

```mojo
@fieldwise_init
struct Point:
    var x: Int

def pick(p: Point) -> Int:
    return p.x

def pick(a: Int, b: Int) -> Int:
    return a + b

def main():
    print(pick(Point(7)))   # old: runtime error; new: 7
```

Nothing in the test suite caught this because all existing overload coverage
(`assets/ok/overloading_arity.mojo`, checker/vm tests) used only scalar
parameter types, where the two manglings happened to agree.

Consolidating both sides onto one module fixes this **by construction**: both
now spell a type the way its annotation reads (`Point`, `Pair$Int`, `T`).

## 3. The new module: `src/symbol.rs`

Design follows the todo's scope: a representation cleanup, no
overload-semantics change.

### Typed signature data

- **`TypeKey`** — the canonical mangled spelling of one parameter type.
  Constructed only inside the module, via:
  - `TypeKey::from_ast(&ast::Type)` — the **definition side** (MIR/VM name
    functions from declared annotations). Internally `ast_raw`.
  - `TypeKey::from_ty(&types::Ty)` — the **resolution side** (the checker
    records callees from the winning signature's parameter `Ty`s). Internally
    `ty_raw`.
  Both go through the same `sanitize` (non-ASCII-alphanumeric → `$`), same as
  before.
- **`SignatureKey`** — an ordered `Vec<TypeKey>`, built by
  `from_ast_params(&[FnParam])` or `from_tys(impl IntoIterator<Item = &Ty>)`.
  The `$ov$…` suffix formatter is **private** to the module.
- **`function_symbol(base, &SignatureKey)`** / **`method_symbol(type_name,
  method, &SignatureKey)`** — the only public formatters.

### Mangling alignment (deliberate spelling decisions)

`ty_raw` and `ast_raw` keep every spelling from the old code **except** the
arms changed to make the two sides agree. All changes are on paths that
previously produced unresolvable names (so no *working* program's emitted
names change):

| Type | New spelling (both sides) | What changed |
|---|---|---|
| `Ty::Struct(name, args)` | `name[$arg…]` | checker dropped the `Struct$` prefix |
| `Ty::Param { name }` | `name` | checker dropped the `Param$` prefix |
| `ast::Type::SelfParam(name)` | `name` | MIR/VM dropped the `SelfParam$` prefix (aligns with `Ty::Param`, which is what the checker resolves `Self.T` to) |
| `Ty::Pointer(elem)` | `UnsafePointer$elem` | checker: explicit arm instead of the `Display` fallback (which left a trailing `$`) |
| `Ty::Assoc { base, name }` | `Assoc$base$name` | checker: explicit arm matching the AST side (was `Display`: `base$name`) |

Unchanged spellings: scalars (`Int`, `UInt`, `Float64` incl. literal-type
folding, `Bool`, `String`, `None`), `List$elem`, `Tuple$…`, `Self`, value
arguments (`V8`), and both catch-alls (`Ty` → `Display`, `ast::Type` →
`Debug`, sanitized) for the rare types that can't appear in a checkable
overloaded signature (function types, `ref`).

Definition-side value arguments are folded with the same supported comptime
integer operations and top-level constants as the checker. Thus
`FixedBuffer[N]` and `FixedBuffer[2 + 6]` both match the checker-side resolved
`FixedBuffer[8]` symbol; an exact checker-recorded call never incorrectly relies
on the VM's arity fallback. SIMD scalar aliases are normalized by `Ty` display
to the same spelling used by their annotations.

Source-controlled identifier components use an injective escape for punctuation
and Unicode while ordinary ASCII names retain their existing spelling. This is
required now that stropped identifiers are supported: `A-B` and `A_B` must not
both sanitize to `A$B` and silently dispatch to the same overload.

### Overload-set scanning and lowered names

- **`OverloadSets::scan(&[Stmt])`** — one pass replacing the duplicated
  `top_level_overloads` + `method_overloads` (previously two functions × two
  copies). Produces `name → {arities}` maps for free functions and
  `Type.method` names, keeping only names with ≥ 2 definitions. Methods are
  keyed under `lifecycle_method_name` as before (so the Mojo copy constructor
  counts as `__copyinit__`, not an `__init__` overload). Accessors:
  `function_is_overloaded(name, arity)`, `method_is_overloaded(source, arity)`.
  Derives `Default + Clone` because MIR's `Flatten` clones it into `try`
  sub-region lowering, as the old `HashMap` field was.
- **`lowered_def_name(name, params, &sets)`** / **`lowered_method_name(source,
  params, &sets)`** — signature-qualify iff overloaded; otherwise the plain
  source name. Replaces `overloaded_def_name`/`overloaded_method_name`
  (previously two copies each).
- **`lifecycle_method_name(&Method)`** (+ private `is_mojo_copy_constructor`) —
  moved verbatim from the two copies in `mir/mod.rs` and `vm.rs`.
- **`nested_lifted_name(outer, inner)`** → `outer$inner` — the nested-`def`
  lift name, moved from an inline `format!` in `lower_fn_nested`.
- **`unresolved_overload_marker(name, argc)`** → `name#argc` — MIR's poison
  name for an overloaded call with no checker-recorded target (only reachable
  off the checked path); moved from `Flatten::overloaded_name` and documented
  as deliberately unresolvable.

### VM symbol predicates

- **`is_overload_of(symbol, base)`** — `symbol` = `base` + `$ov$…`; replaces
  the VM's hand-built prefix `format!`.
- **`init_overload_struct(symbol)`** — `Type.__init__$ov$…` → `Some("Type")`;
  replaces the VM's `split_once(".__init__$ov$")` reparse.

## 4. Per-file migration details

### `src/checker.rs`

The six helpers collapsed into two thin wrappers (kept under their old names to
minimize call-site churn — three call sites: free-call resolution
`infer_call`/`Ty::Overload`, method resolution `infer_method_call`, constructor
resolution `infer_construction`):

- `callable_lowered_name(name, &Ty)` — extracts `params` from
  `Ty::Func`/`Ty::GenericFunc`, then `symbol::function_symbol` +
  `SignatureKey::from_tys`.
- `method_lowered_name(type_name, method, &MethodSig)` —
  `symbol::method_symbol` over `sig.params`. Note: `sig.params` are the
  **declared, unsubstituted** parameter types (e.g. `Ty::Param` for `Self.T`),
  which is what makes them match the MIR definition side, which mangles the
  declared annotations. The doc comment on the wrapper states this invariant.

No change to overload *resolution* (scoring, ambiguity, `overload_targets`
recording) — only to how the recorded string is built.

### `src/mir/mod.rs`

- `lower_program`: the two scans become one
  `crate::symbol::OverloadSets::scan(program)`; def naming uses
  `symbol::lowered_def_name`, method naming `symbol::lowered_method_name`, and
  the lifecycle rename `symbol::lifecycle_method_name`.
- Plumbing types changed from `HashMap<String, HashSet<usize>>` to
  `&OverloadSets` / `OverloadSets` in: `FunctionLowering.overloads`,
  `lower_cfg_nested`'s parameter, `Flatten.overloads`, and `lower_cfg`'s
  default (`OverloadSets::default()`).
- `Flatten::overloaded_name` (the no-target fallback for free calls) now calls
  `sets.function_is_overloaded` and `symbol::unresolved_overload_marker`, with
  a doc comment explaining the poison-name contract. Behavior identical.
- Nested lift name: `symbol::nested_lifted_name(name, dname)`.
- **Deleted** (~120 lines): `top_level_overloads`, `method_overloads`,
  `overloaded_def_name`, `overloaded_method_name`, `signature_suffix`,
  `type_mangle`, `value_arg_mangle`, `lifecycle_method_name`,
  `is_mojo_copy_constructor`. Unused `Method` import removed.

### `src/backend/vm.rs`

- `Prog::overload_name` (the arity fallback): prefix matching now via
  `symbol::is_overload_of`; otherwise unchanged. Per the task's item 4
  ("retain an explicit fallback only for genuinely unchecked/synthetic paths,
  and document each remaining caller"), its doc comment now enumerates the
  legitimate callers — all VM-synthesized dispatches that have no checked call
  span: `call_dunder` (operator/`__str__`/`__hash__`/`__contains__` dunders),
  `store_index_dunder` (`__setitem__`), the `for`-loop iterator protocol
  (`__next__`, two sites), `__init__` construction reached without a recorded
  target, and `method_call` when `resolved` is `None` (i.e. the method is not
  overloaded; the plain name resolves via the early `index_of` return).
  Checker-resolved calls carry their exact lowered callee and never take this
  path. No fallback was *removed*: auditing each caller showed none of them
  fires where the checker recorded a target (the resolved name is already
  preferred at every such site via `Option` precedence).
- `call_named`: the constructor route now tests
  `symbol::init_overload_struct(name)` instead of string-splitting on
  `".__init__$ov$"`. Same semantics.
- `build_sigs`: keys via `symbol::lowered_def_name` over
  `OverloadSets::scan`.
- `build_structs` / `mut_self_methods`: keys via
  `symbol::lifecycle_method_name` + `symbol::lowered_method_name`. The old
  code inserted the **bare method name** for a non-overloaded `mut self`
  method and the **full qualified name** for an overloaded one; the new code
  preserves exactly that (compares `lowered == source` and falls back to the
  bare name), with a comment explaining why (`method_call` checks the bare
  name when no lowering happened). *Reviewer note:* the old condition was
  `method_overloads.contains_key(&source)` while the new one is
  `method_is_overloaded(&source, m.params.len())` — these are equivalent
  because the arity set for an overloaded name is built from the same
  program's method declarations, so any real method's own arity is always a
  member.
- **Deleted** (~145 lines): the verbatim copies of all nine helpers. Unused
  `FnParam`/`Method`/`ParamArg` imports removed.

### `src/lib.rs`

`pub mod symbol;` — public so integration tests (and future tooling) can use
it; nothing re-exported at the crate root.

## 5. Behavior changes (intended)

1. **Fixed:** overload sets containing struct-typed, generic (`T`), `Self.T`,
   `UnsafePointer`, or `Assoc`-typed parameters now resolve at runtime for
   both free functions and methods (previously a runtime `Unsupported` /
   unknown-method error). Emitted MIR names for overloaded defs with
   `Self.T`-typed params change from `…$ov$SelfParam$T` to `…$ov$T` — but no
   working program could observe the old spelling, since the checker-recorded
   callee never matched it.
2. **Unchanged:** every other emitted name and all overload-resolution
   semantics (scoring, ambiguity, arity handling, keyword rejection). The full
   pre-existing suite (~650 tests incl. 237 checker + 123 vm + asset fixtures)
   passes without modification.

## 6. Tests added

`tests/symbol_test.rs` (8 tests, all through public API — programs are parsed
and lowered rather than hand-building AST):

- `free_function_overloads_get_signature_qualified_names` — pins `pick$ov$`,
  `pick$ov$Int`, `pick$ov$String` (incl. the zero-arg empty suffix).
- `non_overloaded_def_keeps_its_source_name`.
- `method_and_constructor_overloads_get_qualified_names` — pins
  `Box.__init__$ov$…` and `Box.value$ov$…`.
- `mojo_copy_constructor_counts_as_copyinit_not_an_init_overload` — the
  lifecycle rename keeps a single ordinary `__init__` unqualified.
- `struct_and_generic_parameter_types_mangle_from_their_annotations` — pins
  `pick$ov$Point` and `pick$ov$Pair$Int` (the fixed spellings).
- `nested_defs_lift_to_dollar_joined_names` — pins `outer$inner`.
- `checker_recorded_callees_name_real_mir_functions` — **the drift
  regression**: for a program exercising struct/generic/`Self.T`/method
  overloads, every value in `resolve_overload_targets` must be a function name
  `lower_program` actually emits.
- `ov_spelling_appears_only_in_the_symbol_module` — **repository hygiene**
  (task item 5): recursively scans `src/**/*.rs` and fails if the literal
  `$ov$` appears outside `src/symbol.rs`.

`assets/ok/overloading_struct_params.mojo` — end-to-end fixture (parse → check
→ ownership → VM) covering: same-arity type-directed free overloads over
`Point` / `Int` / `Pair[Int]`, overloaded `__init__` (zero-arg and `Int`), and
an overloaded method whose second overload takes a struct parameter. Output:
`7 / 2 / 10 / 5 8 / 0`.

## 7. Documentation updates

- The former todo entry recorded what the module owns, the drift that was fixed,
  the retained fallback and its documented callers, and comptime value-argument
  folding and stropped-name collision coverage. Under the current roadmap
  lifecycle policy, that completed task no longer remains in the task tracker.
- `docs/architecture.md` — "Overloaded Names In MIR" section now names
  `src/symbol.rs` as the owner of the scheme and describes the typed
  `SignatureKey` and the test guard.

## 8. Verification

- `./scripts/check` (rustfmt check, `cargo test`, `cargo clippy --all-targets
  -- -D warnings`) passes.
- Full suite: all pre-existing tests pass unchanged; +8 symbol tests; +1 `ok`
  fixture picked up automatically by `assets_test`.
- Manual repro from §2 confirmed broken before, fixed after.

## 9. Explicitly out of scope (per the task's non-goals)

No changes to coercion ranking, no variadic or return-type overloads, no
source-level mangling redesign, no linker/ABI work, and no removal of the
`#argc` poison-marker mechanism (only relocated and documented). The deeper
"prefer checked declarations over AST-shaped side tables in the VM" item
(todo item 3) remains open — `build_sigs`/`build_structs` still scan the AST;
they just do it through the canonical scan now.

## 10. Suggested review focus

1. The mangling alignment table in §3 — confirm each changed arm matches the
   corresponding arm on the other side (`ast_raw` vs `ty_raw` in
   `src/symbol.rs`).
2. The equivalence argument for `mut_self_methods` keying in §4
   (`contains_key` vs `method_is_overloaded`).
3. `method_lowered_name` using **unsubstituted** `sig.params` — verify no
   checker call site passes a receiver-substituted `MethodSig` (the two sites
   are `infer_construction` and `infer_method_call`; both use the registry
   `sig` directly).
4. That `OverloadSets::scan` is called in three places (MIR `lower_program`,
   VM `build_sigs`, VM `build_structs`) — redundant scans of the same program,
   kept to preserve the existing structure; acceptable cost, but flag if you'd
   rather thread one scan through `build_prog`.
