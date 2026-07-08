from list import List

struct DictEntry[K: Equatable & Copyable & Movable, V: Copyable & Movable](Copyable):
    var key: Self.K
    var value: Self.V

    def __init__(out self, key: Self.K, value: Self.V):
        self.key = key
        self.value = value

struct Dict[K: Equatable & Copyable & Movable, V: Copyable & Movable]:
    var entries: List[DictEntry[Self.K, Self.V]]

    def __init__(out self):
        self.entries = List[DictEntry[Self.K, Self.V]]()

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
        raise "missing key"

    def __setitem__(mut self, key: Self.K, value: Self.V):
        var i: Int = self.find_index(key)
        if i >= 0:
            self.entries[i] = DictEntry[Self.K, Self.V](key, value)
        else:
            self.entries.append(DictEntry[Self.K, Self.V](key, value))

    def get_or(self, key: Self.K, default: Self.V) -> Self.V:
        var i: Int = self.find_index(key)
        if i >= 0:
            return self.entries[i].value
        return default

    def __len__(self) -> Int:
        return len(self.entries)

    def __iter__(self) -> List[DictEntry[Self.K, Self.V]]:
        return self.entries
