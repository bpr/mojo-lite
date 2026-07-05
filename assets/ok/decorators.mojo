# General decorators (parsed; unmodeled ones ignored) + a dunder method.
@always_inline
def twice(x: Int) -> Int:
    return x + x

@value
@fieldwise_init
struct Point:
    var x: Int
    var y: Int

    def sum(self) -> Int:
        return self.x + self.y

def main():
    var p: Point = Point(3, 4)
    print(twice(p.sum()))
