# Todo

This is a living list of work that is too concrete to bury in the architecture
document, but still broader than a single test failure.

## Stabilization Checkpoint

Status: highest priority.

Yes: this is a good time to stop and pay down tech debt before the next language
feature wave. The compiler has just absorbed several foundational changes:
signature-aware function/method overloading, hashable/hash-backed collection
proofs, numeric operation traits, self-hosted math helpers, and lifecycle marker
traits with observable copy/move/drop behavior. More self-hosting will now put
pressure on exactly the parts of the compiler that have become a little dense.

The first checkpoint should be mechanical and bounded:

- Status: pending.
  Make `cargo clippy --all-targets -- -D warnings` clean. Current failures are
  small enough to handle directly: complex checker types need names, one checker
  branch wants `?`, one membership check wants `contains`, one nested `if` can be
  collapsed, and one MIR helper has too many arguments.

- Status: pending.
  Add clippy to the normal local gate next to `cargo fmt` and `cargo test`.

- Status: pending.
  Factor complex checker data shapes into named aliases or small structs. This is
  especially useful around overload resolution and resolved-callee tables.

- Status: pending.
  Consolidate overload signature and lowered-name construction. Checker, MIR,
  and VM must agree on names such as `pick$ov$Int`; duplicated string logic is
  likely to drift as type-directed overloading grows.

- Status: pending.
  Keep moving runtime/MIR metadata toward checked declarations rather than
  rebuilding meaning from AST-shaped side tables in the VM.

- Status: pending.
  Improve trait and marker-trait diagnostics. A failed `Copyable`,
  `ImplicitlyCopyable`, `Hashable`, or numeric-operation bound should explain the
  missing operation or field that caused the failure where possible.

- Status: pending.
  Review untracked planning scratch files before the next commit and either
  commit them intentionally or remove them intentionally.

## Module System And Stdlib Layout

Status: high priority.

The current module linker is useful but too path-shaped. It supports
`from module import Name` and relative imports, then hoists declarations into one
flat program. That was enough to get self-hosted `stdlib/` files working, but it
is not the organization mojo-lite wants long-term.

The next direction should follow Mojo's file organization more closely:

```text
stdlib/
  std/
    collections/
      list.mojo
      set.mojo
      dict.mojo
    optional.mojo
```

Then user code and fixtures should be able to write imports like:

```mojo
from std.collections.dict import Dict
from std.collections.list import List
from std.optional import Optional
```

They should not need repository-relative imports such as:

```mojo
from ...stdlib.dict import Dict
```

That syntax is a symptom that the linker has no standard-library search root.

### Tasks

- Status: pending.
  Add a module search path concept to `src/module.rs`.

- Status: pending.
  Make the default search roots include the directory of the importing file and
  the repository/compiler stdlib root.

- Status: pending.
  Move or mirror the current self-hosted library into a Mojo-like layout under
  `stdlib/std/`.

- Status: pending.
  Update self-hosted fixtures to import through `std...` paths instead of
  relative-dot paths.

- Status: pending.
  Decide whether old imports such as `from list import List` remain supported as
  compatibility shims or disappear once the stdlib layout moves.

- Status: pending.
  Add module tests for stdlib-root lookup, dotted paths, transitive imports, and
  missing imported names.

### Likely Implementation Shape

`src/module.rs` currently resolves a module by starting from the importing file's
directory:

```rust
fn module_file(from_dir: &Path, level: usize, path: &[String])
```

That function probably needs to become a resolver that can try multiple roots:

```text
relative import with dots:
  resolve from the importing file's directory

absolute import:
  try importing file's directory
  then try configured stdlib roots
```

The public API can stay small. One option:

```rust
pub fn link(entry_path: &Path) -> Result<Vec<Stmt>, ModuleError>
pub fn link_with_options(entry_path: &Path, options: LinkOptions) -> Result<Vec<Stmt>, ModuleError>

pub struct LinkOptions {
    pub search_roots: Vec<PathBuf>,
}
```

`link(entry_path)` can construct default options:

- the entry file's directory, so local examples keep working
- `CARGO_MANIFEST_DIR/stdlib`, so `from std.collections.dict import Dict` works
  in tests and the CLI when run from this repo

Longer term, the CLI may also want a `--stdlib` or `--module-path` option.

### Acceptance Sketch

Create a fixture like:

```mojo
from std.collections.dict import Dict

def main():
    var d: Dict[String, Int] = Dict[String, Int]()
    d["a"] = 1
    print(d["a"])
```

The asset harness should run this through the linker and pass without any
leading dots.

## Asset Harness Uses Linking

Status: implemented.

The file-based asset harness should run fixtures through `mojo_lite::link(path)`
instead of parse-only `parse(source)`. Otherwise imports parse successfully but
remain no-ops, and imported names fail later as undefined variables.

Keep this behavior. It lets `assets/ok/*.mojo` files exercise modules the same
way CLI file execution does.

## Self-Hosted Collections

Status: active.

The current self-hosted proofs are:

- `Optional`
- `List`
- `Set`
- list-backed `Dict`
- experimental hash-backed `HashSet`
- hashing helpers

Next useful work:

- Status: pending.
  Expand `Dict` tests for string values, overwrites, missing keys, value
  semantics, and iteration over entries.

- Status: pending.
  Decide whether `DictEntry` is public API or hidden behind future key/value/item
  views.

- Status: pending.
  Keep the list-backed `Dict` as a reference implementation even if a
  hash-backed collection appears later.

- Status: pending.
  Add a hash-backed `Dict` only after the stabilization checkpoint. Use the
  existing list-backed `Dict` as the behavior oracle and keep collision
  resolution explicit and testable.

## Comptime Stress Tests

Status: active, demand-driven.

Direct comptime facts, associated comptime members, and VM-backed CTFE are now
real enough for self-hosted library code to use. The next comptime work should be
pulled by self-hosted code, not guessed in isolation.

- Status: implemented.
  Add a small self-hosted algorithm module that uses direct comptime facts:
  `is_same_type`, value-parameter constants, associated `comptime` members, and
  VM-backed CTFE.

- Status: blocked until demanded.
  Implement deeper nested generic CTFE helper specialization when real stdlib
  code needs `outer[T]()` calling `inner[T]()` where `inner` reads `T` facts.

## Traits

Status: active, demand-driven.

Trait names are recognized more broadly than they are semantically implemented.
Do not flesh them out all at once.

- Status: implemented foundation.
  Make `Iterable` / `Iterator` useful with associated `Element` facts in generic
  self-hosted algorithms.

- Status: implemented.
  Make `Comparable` enable ordering on opaque type parameters, with a negative
  test proving `Equatable` alone is not enough.

- Status: implemented.
  Make `Sized` enable `len(x)` on opaque type parameters.

- Status: implemented for `Hashable`; `Hasher` deferred.
  `Hashable` permits `__hash__() -> UInt` on bounded opaque values and built-in
  scalar values. It intentionally does not imply `Equatable`. `Hasher` remains a
  future incremental-hashing protocol.

- Status: implemented.
  Numeric operation traits gate generic use of `abs`, `round`, `**`,
  conversions, `Bool`, `divmod`, and self-hosted math helpers.

- Status: implemented for lifecycle markers.
  `Copyable`, `ImplicitlyCopyable`, `ImplicitlyDeletable`, and `Movable` are no
  longer just accepted names; they line up with the current ownership model.
  `RegisterPassable` and `TrivialRegisterPassable` remain deferred backend/layout
  markers.

- Status: deferred until demanded.
  Implement general trait default methods. `Hashable` currently works through an
  intrinsic plus explicit `__hash__`; inherited default bodies should wait until
  a library protocol actually needs them.

## Overloading

Status: implemented foundation; needs cleanup before expansion.

Function, method, and constructor calls now resolve fixed-arity overloads and
conservative same-arity type-directed overloads. The checker records the
resolved callee, and MIR/VM lowering uses signature-qualified names.

- Status: pending cleanup.
  Centralize signature keys and lowered-name formatting.

- Status: pending.
  Add more negative tests for duplicate-equivalent overloads and ambiguous
  coercion paths.

- Status: deferred until demanded.
  Extend overload ranking beyond the current conservative exact/coercion model.

## Documentation

Status: ongoing.

- Status: pending.
  Update `docs/architecture.md` once module search roots and the stdlib layout
  exist.

- Status: pending.
  Update `stdlib/README.md` after the stdlib moves to `stdlib/std/...`.

- Status: pending.
  Keep `roadmap.md` focused on phase order and this file focused on concrete
  backlog items.
