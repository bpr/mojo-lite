# Subscript *assignment* `c[i] = e` on a user struct dispatches to
# `__setitem__(mut self, i, e)`, with the mutation written back to the receiver.
# The read half of `c[i] += e` dispatches to `__getitem__`.
@fieldwise_init
struct Pair:
    var a: Int
    var b: Int

    def __getitem__(self, i: Int) -> Int:
        if i == 0:
            return self.a
        return self.b

    def __setitem__(mut self, i: Int, v: Int):
        if i == 0:
            self.a = v
        else:
            self.b = v

@fieldwise_init
struct Holder:
    var p: Pair

def main():
    var p: Pair = Pair(1, 2)
    p[0] = 10
    p[1] = 20
    print(p[0], p[1])
    p[0] += 5
    print(p[0])
    # A nested place: the write persists back through the outer struct.
    var h: Holder = Holder(Pair(5, 6))
    h.p[1] = 99
    print(h.p[0], h.p[1])
