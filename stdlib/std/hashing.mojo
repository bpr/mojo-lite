# A tiny self-hosted hash helper (Phase 6). `bucket_index` maps a hashable key
# into `[0, bucket_count)` using its `__hash__` — the reference building block a
# hash-backed collection uses to choose a bucket.
#
# The hash of a key is a `UInt` (mojo-lite's native word-sized unsigned integer),
# matching Mojo's `Hashable.__hash__(self) -> UInt`. Built-in scalar keys
# (`Int`, `String`, …) hash intrinsically; a user key struct provides its own
# `__hash__`.

def bucket_index[K: Hashable](key: K, bucket_count: Int) -> Int:
    return Int(key.__hash__() % UInt(bucket_count))
