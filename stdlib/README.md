# stdlib — the standard library, written in mojito itself

These are ordinary mojito `.mojo` files (no compiler intrinsic): the north-star
proof that the language is expressive enough to author its own collections and
small generic algorithms.

The preferred import shape follows Mojo's `std` package layout:

```mojo
from std.collections.list import List
from std.collections.set import Set
from std.collections.dict import Dict
from std.optional import Optional
from std.math import floor, ceil
```

The older flat files (`stdlib/list.mojo`, `stdlib/dict.mojo`, and friends)
remain as compatibility mirrors for now, so legacy examples such as
`from list import List` still work when `stdlib/` is on the search path.

- `std/collections/list.mojo` — a generic, growable `List[T]` backed by an `UnsafePointer[T]`, with
  the full value-type lifecycle (`__init__`/`__copyinit__`/`__moveinit__`), subscript
  read/write (`__getitem__`/`__setitem__`), `__len__`, and the iterator protocol
  (`__iter__` → `_ListIter[T]` with `__next__`/`__len__`). Growth reallocs the buffer.
- `std/optional.mojo` — a generic `Optional[T]` using zero-or-one value storage,
  including an empty constructor for generic absent values.
- `std/iterable.mojo` — minimal self-hosted `Iterator` and `Iterable` traits. They
  expose associated compile-time `Element` facts, and `Iterable` also exposes an
  associated `Iter` type so containers can return a separate iterator object.
- `std/collections/set.mojo` — a generic, list-backed `Set[T]` for `Equatable & Copyable & Movable`
  elements. It supports `add`, membership through `in`/`__contains__`, `len`, and
  iteration by returning its backing `List[T]`. It conforms to `Iterable`.
- `std/collections/dict.mojo` — a generic, insertion-ordered, list-backed
  `Dict[K, V]`. It supports subscripts, overloaded `get`, membership, key
  iteration, eager `keys`/`values`/`items` snapshots, public `DictEntry`, and
  value-semantic copying. A missing subscript raises `Error("missing key")`.
- `std/collections/hashdict.mojo` — a hash-backed, insertion-ordered
  `HashDict[K, V]`: dense entries preserve order while `List[List[Int]]` buckets
  index them. It grows and rehashes explicitly and mirrors the `Dict` API.
- `std/algorithms.mojo` — small generic helpers that exercise comptime-guided library
  code: type predicates, CTFE-computed constants, value parameters, and associated
  compile-time facts. It includes `first_or[C: Iterable]`, which consumes
  `C.Element` through an opaque iterable bound.
- `std/hashing.mojo` — a tiny hash helper: `bucket_index[K: Hashable](key, bucket_count)`
  maps a key into `[0, bucket_count)` via its `__hash__` (`-> UInt`). Built-in
  scalar keys hash intrinsically; the hash is deterministic (no per-run seed).
- `std/math.mojo` — self-hosted numeric rounding helpers `floor`/`ceil`/`trunc`/`ceildiv`,
  each generic over its Mojo trait bound (`Floorable`/`Ceilable`/`Truncable`/`CeilDivable`).
  Unlike `abs`/`round`/`divmod` (Mojo prelude builtins, available bare), these mirror
  Mojo's `math` module and must be imported: `from std.math import floor`. Built-in `Int`/`Float64`
  supply the underlying dunders intrinsically.
- `std/collections/hashset.mojo` — an experimental hash-backed `HashSet[T: Hashable & Equatable &
  Copyable & Movable]`. It keeps a fixed array of buckets and only scans the bucket
  a key hashes into, so it is genuinely hash-backed (unlike the linear-scan `Set`).
  `Hashable` does not imply `Equatable`, so both bounds are named — the hash picks a
  bucket, equality resolves collisions within it. Its nested buckets use the
  self-hosted `List`; the bucket count remains fixed pending a rehashing follow-up.

Underscore-prefixed structs such as `_ListIter` are implementation details,
following the Python convention that Mojo currently inherits. `DictEntry` is
public, matching Mojo's item-view element. Views are eager snapshots rather than
reference views until origins and reference iterators are implemented.

The register VM executes these directly; `tests/self_host_test.rs` links and runs
them. As the language gains features, more of the library moves out of Rust and into
this directory (eventually retiring the built-in `List`/`Tuple` intrinsics).
