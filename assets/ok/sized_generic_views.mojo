# Phase 5: a `Sized` bound lets a generic helper call `len(x)` on an opaque type
# parameter. The concrete type's `__len__` runs at runtime after type erasure —
# here a built-in `List` and a user struct that declares `Sized`.
def is_empty[T: Sized](x: T) -> Bool:
    return len(x) == 0

def has_two_or_more[T: Sized](x: T) -> Bool:
    return len(x) >= 2

struct Bag(Sized):
    var items: List[Int]

    def __init__(out self):
        self.items = List[Int]()

    def add(mut self, v: Int):
        self.items.append(v)

    def __len__(self) -> Int:
        return len(self.items)

def main():
    var xs: List[Int] = [1, 2, 3]
    var empty: List[Int] = List[Int]()
    print(is_empty(xs))
    print(is_empty(empty))
    print(has_two_or_more(xs))

    var b: Bag = Bag()
    print(is_empty(b))
    b.add(10)
    b.add(20)
    print(is_empty(b))
    print(has_two_or_more(b))
