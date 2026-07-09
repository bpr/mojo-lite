# Todo

This is a living list of work that is too concrete to bury in the architecture
document, but still broader than a single test failure.

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

## Comptime Stress Tests

Status: active.

The next comptime work should be pulled by self-hosted code.

- Status: pending.
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

- Status: pending.
  Make `Iterable` / `Iterator` useful with associated `Element` facts in generic
  self-hosted algorithms.

- Status: pending.
  Make `Comparable` enable ordering on opaque type parameters, with a negative
  test proving `Equatable` alone is not enough.

- Status: pending.
  Make `Sized` enable `len(x)` on opaque type parameters.

- Status: deferred.
  Delay `Hashable` / `Hasher` until a hash-backed collection proof exists.

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
