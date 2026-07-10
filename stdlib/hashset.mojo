# An experimental hash-backed `HashSet[T]` (Phase 6), written in mojito.
#
# Unlike the list-backed `Set[T]` (which linearly scans every element on each
# `contains`/`add`), this keeps a fixed array of buckets and only scans the one
# bucket a key hashes into — a genuine hash-backed collection. It is a proof of
# the `Hashable` machinery, not (yet) a replacement for the reference `Set`: the
# bucket count is fixed (no rehash/growth).
#
# Keys must be `Hashable` (to choose a bucket) and `Equatable` (to resolve
# collisions within a bucket) — `Hashable` deliberately does not imply
# `Equatable`, so both bounds are named. The buckets use the built-in `List`.

from hashing import bucket_index

struct HashSet[T: Hashable & Equatable & Copyable & Movable]:
    var buckets: List[List[Self.T]]
    var nbuckets: Int
    var count: Int

    def __init__(out self):
        self.nbuckets = 8
        self.count = 0
        self.buckets = List[List[Self.T]]()
        var i: Int = 0
        while i < self.nbuckets:
            self.buckets.append(List[Self.T]())
            i = i + 1

    def contains(self, key: Self.T) -> Bool:
        var idx: Int = bucket_index(key, self.nbuckets)
        for existing in self.buckets[idx]:
            if existing == key:
                return True
        return False

    def add(mut self, key: Self.T):
        if self.contains(key):
            return
        var idx: Int = bucket_index(key, self.nbuckets)
        self.buckets[idx].append(key)
        self.count = self.count + 1

    def __len__(self) -> Int:
        return self.count
