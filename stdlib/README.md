# stdlib — the standard library, written in mojo-lite itself

These are ordinary mojo-lite `.mojo` files (no compiler intrinsic): the north-star
proof that the language is expressive enough to author its own collections and
small generic algorithms. Import them like any module — `from list import List`,
`from optional import Optional`.

- `list.mojo` — a generic, growable `List[T]` backed by an `UnsafePointer[T]`, with
  the full value-type lifecycle (`__init__`/`__copyinit__`/`__moveinit__`), subscript
  read/write (`__getitem__`/`__setitem__`), `__len__`, and the iterator protocol
  (`__iter__` → `_ListIter[T]` with `__next__`/`__len__`). Growth reallocs the buffer.
- `optional.mojo` — a generic `Optional[T]` (a present value or an absent one).
- `iterable.mojo` — minimal self-hosted `Iterator` and `Iterable` traits. They
  expose associated compile-time `Element` facts, and `Iterable` also exposes an
  associated `Iter` type so containers can return a separate iterator object.
- `set.mojo` — a generic, list-backed `Set[T]` for `Equatable & Copyable & Movable`
  elements. It supports `add`, membership through `in`/`__contains__`, `len`, and
  iteration by returning its backing `List[T]`. It conforms to `Iterable`.
- `dict.mojo` — a generic, list-backed `Dict[K, V]` for equatable/copyable/movable
  keys and copyable/movable values. It supports subscript read/write,
  overwrite-in-place, `get_or`, membership, `len`, iteration over entries, and an
  explicit Mojo-style copy constructor so copying a dictionary preserves value
  semantics. A missing key raises `Error("missing key")`.
- `algorithms.mojo` — small generic helpers that exercise comptime-guided library
  code: type predicates, CTFE-computed constants, value parameters, and associated
  compile-time facts. It includes `first_or[C: Iterable]`, which consumes
  `C.Element` through an opaque iterable bound.
- `hashing.mojo` — a tiny hash helper: `bucket_index[K: Hashable](key, bucket_count)`
  maps a key into `[0, bucket_count)` via its `__hash__` (`-> UInt`). Built-in
  scalar keys hash intrinsically; the hash is deterministic (no per-run seed).
- `math.mojo` — self-hosted numeric rounding helpers `floor`/`ceil`/`trunc`/`ceildiv`,
  each generic over its Mojo trait bound (`Floorable`/`Ceilable`/`Truncable`/`CeilDivable`).
  Unlike `abs`/`round`/`divmod` (Mojo prelude builtins, available bare), these mirror
  Mojo's `math` module and must be imported: `from math import floor`. Built-in `Int`/`Float64`
  supply the underlying dunders intrinsically.
- `hashset.mojo` — an experimental hash-backed `HashSet[T: Hashable & Equatable &
  Copyable & Movable]`. It keeps a fixed array of buckets and only scans the bucket
  a key hashes into, so it is genuinely hash-backed (unlike the linear-scan `Set`).
  `Hashable` does not imply `Equatable`, so both bounds are named — the hash picks a
  bucket, equality resolves collisions within it. The bucket count is fixed (no
  rehash yet), so it is a proof of the `Hashable` machinery, not a `Set` replacement.

Underscore-prefixed structs are implementation details, following the Python
convention that Mojo currently inherits. `_ListIter` and `_DictEntry` are visible
to the compiler because mojo-lite does not yet have private declarations, but
callers should not treat them as stable public API. `Dict` iteration returns
entries today because there are not yet separate key/value/item view types.

The register VM executes these directly; `tests/self_host_test.rs` links and runs
them. As the language gains features, more of the library moves out of Rust and into
this directory (eventually retiring the built-in `List`/`Tuple` intrinsics).
