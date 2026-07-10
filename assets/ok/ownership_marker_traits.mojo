# Phase 8: lifecycle marker traits map to observable copy/move/delete behavior.

@fieldwise_init
struct Plain(ImplicitlyCopyable, ImplicitlyDeletable):
    var n: Int

struct ExplicitBox(Copyable, ImplicitlyDeletable):
    var n: Int

    def __init__(out self, n: Int):
        self.n = n

    def __init__(out self, *, copy: Self):
        self.n = copy.n

def duplicate[T: Copyable](x: T) -> T:
    var y: T = x
    return y

def duplicate_implicit[T: ImplicitlyCopyable](x: T) -> T:
    var y: T = x
    return y

def accept_movable[T: Movable](owned x: T):
    pass

def accept_deletable[T: ImplicitlyDeletable](x: T):
    pass

def main():
    var p: Plain = Plain(7)
    var p2: Plain = duplicate_implicit(p)
    var p3: Plain = duplicate(p)
    print(p2.n, p3.n)

    var b: ExplicitBox = ExplicitBox(9)
    var b2: ExplicitBox = duplicate(b)
    print(b2.n)
    accept_movable(b2^)
    accept_deletable(p2)
