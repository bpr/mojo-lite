# A self-hosted, generic growable `List[T]`, backed by an `UnsafePointer[T]` and
# written entirely in mojito. Exercises the whole language stack: parametric
# types, heap pointers, the value-type lifecycle (init/copy/move), the operator/
# subscript/len dunders, and the iterator protocol.

from std.iterable import Iterable, Iterator

struct _ListIter[T: Copyable & Movable](Iterator):
    comptime Element = Self.T

    var data: UnsafePointer[Self.T]
    var size: Int
    var idx: Int

    def __init__(out self, data: UnsafePointer[Self.T], size: Int):
        self.data = data
        self.size = size
        self.idx = 0

    def __len__(self) -> Int:
        return self.size - self.idx

    def __next__(mut self) -> Self.T:
        var v: Self.T = self.data[self.idx]
        self.idx = self.idx + 1
        return v

struct List[T: Copyable & Movable](Copyable, Iterable):
    comptime Element = Self.T
    comptime Iter = _ListIter[Self.T]

    var data: UnsafePointer[Self.T]
    var size: Int
    var cap: Int

    def __init__(out self):
        self.cap = 4
        self.size = 0
        self.data = UnsafePointer[Self.T].alloc(4)

    def __init__(out self, *, copy: Self):
        self.cap = copy.cap
        self.size = copy.size
        self.data = UnsafePointer[Self.T].alloc(copy.cap)
        var i: Int = 0
        while i < copy.size:
            self.data[i] = copy.data[i]
            i = i + 1

    def copy(self) -> Self:
        return List[Self.T](copy: self)

    def __moveinit__(out self, owned existing: Self):
        self.cap = existing.cap
        self.size = existing.size
        self.data = existing.data

    def grow(mut self):
        var new_cap: Int = self.cap * 2
        var new_data: UnsafePointer[Self.T] = UnsafePointer[Self.T].alloc(new_cap)
        var i: Int = 0
        while i < self.size:
            new_data[i] = self.data[i]
            i = i + 1
        self.data.free()
        self.data = new_data
        self.cap = new_cap

    def append(mut self, v: Self.T):
        if self.size == self.cap:
            self.grow()
        self.data[self.size] = v
        self.size = self.size + 1

    def __len__(self) -> Int:
        return self.size

    def __getitem__(self, i: Int) -> Self.T:
        return self.data[i]

    def __setitem__(mut self, i: Int, v: Self.T):
        self.data[i] = v

    def __iter__(self) -> _ListIter[Self.T]:
        return _ListIter[Self.T](self.data, self.size)
