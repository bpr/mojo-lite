trait Iterator:
    comptime Element: AnyType
    def __next__(mut self) -> Self.Element:
        ...

@fieldwise_init
struct IntIter(Iterator):
    comptime Element = Int
    var n: Int

    def __next__(mut self) -> Int:
        return self.n

def main():
    var it: IntIter = IntIter(7)
    print(it.__next__())
