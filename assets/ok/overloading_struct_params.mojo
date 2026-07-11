# Type-directed overloads whose signatures include struct, generic, and
# `Self.T`-typed parameters. These used to fail at runtime: the checker and MIR
# mangled struct types differently (`pick$ov$Struct$Point` vs `pick$ov$Point`),
# so the recorded callee named no function. The canonical symbol module keeps
# the two spellings identical.

@fieldwise_init
struct Point:
    var x: Int

@fieldwise_init
struct Pair[T: AnyType]:
    var a: Self.T
    var b: Self.T

struct Box:
    var n: Int

    def __init__(out self):
        self.n = 0

    def __init__(out self, n: Int):
        self.n = n

    def get(self) -> Int:
        return self.n

    def get(self, p: Point) -> Int:
        return self.n + p.x

def pick(p: Point) -> Int:
    return p.x

def pick(n: Int) -> Int:
    return n + 1

def pick(q: Pair[Int]) -> Int:
    return q.a

def main():
    print(pick(Point(7)))
    print(pick(1))
    print(pick(Pair(10, 20)))
    var b: Box = Box(5)
    print(b.get(), b.get(Point(3)))
    var empty: Box = Box()
    print(empty.get())
