# A hash-backed, insertion-ordered dictionary implemented in mojito.
#
# Dense entries preserve insertion order; a nested-list index maps each hash
# bucket to entry positions. Views are eager value-semantic snapshots until
# origins and reference iterators are implemented.

from dict import DictEntry
from list import List
from hashing import bucket_index
from iterable import Iterable
from optional import Optional

struct HashDict[K: Hashable & Equatable & Copyable & Movable, V: Copyable & Movable](Copyable, Iterable):
    comptime Element = Self.K
    comptime Iter = List[Self.K]

    var entries: List[DictEntry[Self.K, Self.V]]
    var index: List[List[Int]]
    var nbuckets: Int
    var count: Int

    def __init__(out self):
        self.entries = List[DictEntry[Self.K, Self.V]]()
        self.index = List[List[Int]]()
        self.nbuckets = 8
        self.count = 0
        var i: Int = 0
        while i < self.nbuckets:
            self.index.append(List[Int]())
            i = i + 1

    def __init__(out self, *, copy: Self):
        self.entries = List[DictEntry[Self.K, Self.V]](copy: copy.entries)
        self.index = List[List[Int]](copy: copy.index)
        self.nbuckets = copy.nbuckets
        self.count = copy.count

    def copy(self) -> Self:
        return HashDict[Self.K, Self.V](copy: self)

    def find_index(self, key: Self.K) -> Int:
        var bucket: Int = bucket_index(key, self.nbuckets)
        for entry_index in self.index[bucket]:
            if self.entries[entry_index].key == key:
                return entry_index
        return -1

    def __contains__(self, key: Self.K) -> Bool:
        return self.find_index(key) >= 0

    def __getitem__(self, key: Self.K) -> Self.V:
        var i: Int = self.find_index(key)
        if i >= 0:
            return self.entries[i].value
        raise Error("missing key")

    def __setitem__(mut self, key: Self.K, value: Self.V):
        var existing: Int = self.find_index(key)
        if existing >= 0:
            self.entries[existing] = DictEntry[Self.K, Self.V](key, value)
            return

        var entry_index: Int = len(self.entries)
        self.entries.append(DictEntry[Self.K, Self.V](key, value))
        var bucket: Int = bucket_index(key, self.nbuckets)
        self.index[bucket].append(entry_index)
        self.count = self.count + 1
        if self.count == self.nbuckets:
            self.rehash(self.nbuckets * 2)

    def rehash(mut self, new_bucket_count: Int):
        var new_index: List[List[Int]] = List[List[Int]]()
        var i: Int = 0
        while i < new_bucket_count:
            new_index.append(List[Int]())
            i = i + 1
        i = 0
        while i < len(self.entries):
            var bucket: Int = bucket_index(self.entries[i].key, new_bucket_count)
            new_index[bucket].append(i)
            i = i + 1
        self.index = new_index
        self.nbuckets = new_bucket_count

    def bucket_count(self) -> Int:
        return self.nbuckets

    def get(self, key: Self.K) -> Optional[Self.V]:
        var i: Int = self.find_index(key)
        if i >= 0:
            return Optional[Self.V](self.entries[i].value, True)
        return Optional[Self.V]()

    def get(self, key: Self.K, default: Self.V) -> Self.V:
        var i: Int = self.find_index(key)
        if i >= 0:
            return self.entries[i].value
        return default

    def __len__(self) -> Int:
        return self.count

    def keys(self) -> List[Self.K]:
        var result: List[Self.K] = List[Self.K]()
        for entry in self.entries:
            result.append(entry.key)
        return result

    def values(self) -> List[Self.V]:
        var result: List[Self.V] = List[Self.V]()
        for entry in self.entries:
            result.append(entry.value)
        return result

    def items(self) -> List[DictEntry[Self.K, Self.V]]:
        return self.entries

    def __iter__(self) -> List[Self.K]:
        return self.keys()

