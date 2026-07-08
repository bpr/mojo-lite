# A hand-written `def __init__(out self, …)` constructs a struct without
# `@fieldwise_init`: the body assigns every field (definite initialization), and
# arguments are coerced to the parameter types.
struct Point:
    var x: Int
    var y: Int

    def __init__(out self, x: Int, y: Int):
        self.x = x
        self.y = y

    def sum(self) -> Int:
        return self.x + self.y

struct Scaled:
    var v: Float64

    def __init__(out self, n: Float64):
        self.v = n * 2.0

def main():
    var p: Point = Point(3, 4)
    print(p.x, p.y, p.sum())
    var s: Scaled = Scaled(5)
    print(s.v)
