# Phase 7: numeric-operation trait bounds enable the matching builtin/operator on
# an opaque type parameter. The concrete numeric type's implementation runs after
# type erasure, so these helpers work for `Int`/`Float64` arguments.
def absolute[T: Absable](x: T) -> T:
    return abs(x)

def rounded[T: Roundable](x: T) -> T:
    return round(x)

def powit[T: Powable](x: T, y: T) -> T:
    return x ** y

def to_int[T: Intable](x: T) -> Int:
    return Int(x)

def to_flt[T: Floatable](x: T) -> Float64:
    return Float64(x)

def main():
    print(absolute(-7))
    print(absolute(-3.5))
    print(rounded(2.7))
    print(powit(2, 10))
    print(to_int(3.9))
    print(to_flt(4))
