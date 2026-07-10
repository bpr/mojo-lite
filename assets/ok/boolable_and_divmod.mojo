# Phase 7 (deferred items, now implemented): `Boolable` -> `Bool(x)` and
# `DivModable` -> `divmod(a, b)`, both prelude builtins usable on an opaque type
# parameter and on concrete numerics. `Bool(x)` is truthiness; `divmod` returns
# the `(a // b, a % b)` pair as a Tuple (Python flooring).
def truthy[T: Boolable](x: T) -> Bool:
    return Bool(x)

def quotient_rem[T: DivModable](a: T, b: T) -> Tuple[T, T]:
    return divmod(a, b)

def main():
    print(truthy(0), truthy(5))
    print(truthy(0.0), truthy(2.5))
    var p: Tuple[Int, Int] = quotient_rem(7, 2)
    print(p[0], p[1])
    var q: Tuple[Int, Int] = quotient_rem(-7, 2)
    print(q[0], q[1])
