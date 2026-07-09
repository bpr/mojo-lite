trait Iterable:
    comptime Element: AnyType

@fieldwise_init
struct Bag[T: AnyType](Iterable):
    comptime Element = Self.T
    var value: Self.T

def consume[C: Iterable](c: C) -> C.Element:
    for item in c:
        return item
    raise "empty"
