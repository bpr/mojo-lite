# expect: argument to 'len'
# Phase 5: a plain `T: AnyType` promises no length, so `len(x)` on it is a type
# error. Only a `Sized` (or `SizedRaising`) bound permits `len` on an opaque `T`.
def is_empty[T: AnyType](x: T) -> Bool:
    return len(x) == 0

def main():
    print(is_empty(5))
