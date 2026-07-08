# Lifecycle copy/move: a pointer-owning struct defines `__copyinit__` (deep-copy
# the buffer, so copies are independent) and `__moveinit__` (relocate). Defining
# `__copyinit__` is what makes the type Copyable. Without copyinit the default clone
# would alias the buffer — the wrong value semantics.
struct Buf:
    var data: UnsafePointer[Int]
    var n: Int

    def __init__(out self, n: Int):
        self.data = UnsafePointer[Int].alloc(n)
        self.n = n
        var i: Int = 0
        while i < n:
            self.data[i] = 0
            i = i + 1

    def __copyinit__(out self, existing: Buf):
        self.n = existing.n
        self.data = UnsafePointer[Int].alloc(existing.n)
        var i: Int = 0
        while i < existing.n:
            self.data[i] = existing.data[i]
            i = i + 1

    def __moveinit__(out self, owned existing: Buf):
        self.n = existing.n
        self.data = existing.data

    def set(mut self, i: Int, v: Int):
        self.data[i] = v

    def get(self, i: Int) -> Int:
        return self.data[i]

def main():
    var a: Buf = Buf(2)
    a.set(0, 100)
    var b: Buf = a           # __copyinit__ → independent buffer
    b.set(0, 999)
    print(a.get(0), b.get(0))
    var c: Buf = b^          # __moveinit__ → relocate b into c
    print(c.get(0))
