# stdlib — the standard library, written in mojo-lite itself

These are ordinary mojo-lite `.mojo` files (no compiler intrinsic): the north-star
proof that the language is expressive enough to author its own collections. Import
them like any module — `from list import List`, `from optional import Optional`.

- `list.mojo` — a generic, growable `List[T]` backed by an `UnsafePointer[T]`, with
  the full value-type lifecycle (`__init__`/`__copyinit__`/`__moveinit__`), subscript
  read/write (`__getitem__`/`__setitem__`), `__len__`, and the iterator protocol
  (`__iter__` → `ListIter[T]` with `__next__`/`__len__`). Growth reallocs the buffer.
- `optional.mojo` — a generic `Optional[T]` (a present value or an absent one).

The register VM executes these directly; `tests/self_host_test.rs` links and runs
them. As the language gains features, more of the library moves out of Rust and into
this directory (eventually retiring the built-in `List`/`Tuple` intrinsics).
