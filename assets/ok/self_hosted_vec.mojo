# A growable integer vector written in mojito itself, backed by an
# `UnsafePointer[Int]` — the first proof that the language can express a
# heap-owning container (the Phase 2 / self-hosting milestone). `push` mutates the
# shared storage *through* the pointer (which aliases across the value-type copy).
struct IntVec:
    var data: UnsafePointer[Int]
    var size: Int
    var cap: Int

    def __init__(out self, cap: Int):
        self.data = UnsafePointer[Int].alloc(cap)
        self.size = 0
        self.cap = cap

    def push(mut self, v: Int):
        self.data[self.size] = v
        self.size = self.size + 1

    def get(self, i: Int) -> Int:
        return self.data[i]

    def __len__(self) -> Int:
        return self.size

def main():
    var xs: IntVec = IntVec(8)
    xs.push(3)
    xs.push(1)
    xs.push(4)
    var total: Int = 0
    for i in range(len(xs)):
        total = total + xs.get(i)
    print(len(xs), total)
