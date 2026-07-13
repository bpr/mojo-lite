# Views are eager insertion-ordered snapshots. Mojo's views are lazy references;
# mojito will converge on that behavior when origins/reference iterators exist.
from std.collections.list import List
from std.iterable import Iterable
from std.optional import Optional

struct DictEntry[K: Equatable & Copyable & Movable, V: Copyable & Movable](Copyable):
    var key: Self.K
    var value: Self.V

    def __init__(out self, key: Self.K, value: Self.V):
        self.key = key
        self.value = value

struct Dict[K: Equatable & Copyable & Movable, V: Copyable & Movable](Copyable, Iterable):
    comptime Element = Self.K
    comptime Iter = List[Self.K]

    var entries: List[DictEntry[Self.K, Self.V]]

    def __init__(out self):
        self.entries = List[DictEntry[Self.K, Self.V]]()

    def __init__(out self, *, copy: Self):
        self.entries = List[DictEntry[Self.K, Self.V]](copy: copy.entries)

    def copy(self) -> Self:
        return Dict[Self.K, Self.V](copy: self)

    def find_index(self, key: Self.K) -> Int:
        var i: Int = 0
        while i < len(self.entries):
            if self.entries[i].key == key:
                return i
            i = i + 1
        return -1

    def __contains__(self, key: Self.K) -> Bool:
        return self.find_index(key) >= 0

    def __getitem__(self, key: Self.K) -> Self.V:
        var i: Int = self.find_index(key)
        if i >= 0:
            return self.entries[i].value
        raise Error("missing key")

    def __setitem__(mut self, key: Self.K, value: Self.V):
        var i: Int = self.find_index(key)
        if i >= 0:
            self.entries[i] = DictEntry[Self.K, Self.V](key, value)
        else:
            self.entries.append(DictEntry[Self.K, Self.V](key, value))

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
        return len(self.entries)

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
