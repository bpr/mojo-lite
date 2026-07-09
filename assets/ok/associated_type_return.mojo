trait HasElement:
    comptime Element: AnyType
    def get(self) -> Self.Element:
        ...

@fieldwise_init
struct Box[T: Copyable & Movable](HasElement):
    comptime Element = Self.T
    var value: Self.T

    def get(self) -> Self.T:
        return self.value

def get[C: HasElement](c: C) -> C.Element:
    return c.get()

def main():
    var n: Int = get(Box[Int](1))
    print(n)
