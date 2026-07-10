# Phase 4: `Comparable` turns `<`/`<=`/`>`/`>=` into an ordering contract for an
# opaque type parameter. `& Copyable` lets the helpers return `T` by value under
# the move-only rule.
def min_value[T: Comparable & Copyable](a: T, b: T) -> T:
    if b < a:
        return b
    return a

def clamp[T: Comparable & Copyable](x: T, lo: T, hi: T) -> T:
    if x < lo:
        return lo
    if x > hi:
        return hi
    return x

def main():
    print(min_value(3, 5))
    print(min_value(9, 2))
    print(clamp(10, 0, 7))
    print(clamp(-3, 0, 7))
    print(min_value(2.5, 1.5))
