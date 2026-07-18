trait HasElement:
    comptime Element: AnyType

trait HasCopyableElement:
    comptime Element: Copyable

trait Collection(HasElement, HasCopyableElement):
    def size(self) -> Int: ...

@fieldwise_init
struct IntCollection(Collection):
    comptime Element = Int
    var value: Int

    def size(self) -> Int:
        return 1

@fieldwise_init
struct Offset(Indexer):
    var value: Int

    def __mlir_index__(self) -> Int:
        return self.value

@fieldwise_init
struct Point(Writable, Hashable):
    var x: Int
    var y: Int

    def write_to(self, mut writer: Some[Writer]):
        writer.write("(", self.x, ", ", self.y, ")")

    def write_repr_to(self, mut writer: Some[Writer]):
        writer.write("Point[x=", self.x, ", y=", self.y, "]")

    def __hash__(self, mut hasher: Some[Hasher]):
        hasher.update(self.x)
        hasher.update(self.y)

@fieldwise_init
struct Wrapper[T: AnyType](Writable where conforms_to(T, Writable)):
    var value: Self.T

def main():
    var values = [3, 7]
    var point = Point(2, 5)
    print(values[Offset(1)])
    print(point)
    print(repr(point))
    print("{} {!r}".format(point, point))
    print(hash(point) == hash(Point(2, 5)))
    print(Wrapper[Int](9))
