# expect: argument to 'Bool'
# Phase 7: `Bool(x)` on an opaque type parameter needs a `Boolable` bound. A
# plain `T: AnyType` promises no boolean conversion, so this is a type error.
def truthy[T: AnyType](x: T) -> Bool:
    return Bool(x)

def main():
    print(truthy(5))
