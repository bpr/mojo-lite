# Phase 6: `Hashable` is a real bound — `key.__hash__()` (→ `UInt`) works on an
# opaque `K: Hashable`, and on concrete built-ins so a helper can bucket keys.
# The hash is deterministic, so equal keys land in the same bucket every run.
def bucket_index[K: Hashable](key: K, bucket_count: Int) -> Int:
    return Int(key.__hash__() % UInt(bucket_count))

def main():
    var a: Int = 42
    var b: Int = 42
    var c: Int = 7
    # Equal keys bucket identically; the index is always in range.
    print(bucket_index(a, 8) == bucket_index(b, 8))
    print(bucket_index(c, 8) >= 0)
    print(bucket_index("mojo", 16) == bucket_index("mojo", 16))
