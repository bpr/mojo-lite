# Operator overloading: a user struct participates in `+`, `==`, and `String()`
# via its `__add__`/`__eq__`/`__str__` dunder methods.
@fieldwise_init
struct Vec2:
    var x: Int
    var y: Int

    def __add__(self, other: Vec2) -> Vec2:
        return Vec2(self.x + other.x, self.y + other.y)

    def __eq__(self, other: Vec2) -> Bool:
        return self.x == other.x and self.y == other.y

    def __str__(self) -> String:
        return "Vec2(" + String(self.x) + ", " + String(self.y) + ")"

def main():
    var a: Vec2 = Vec2(1, 2)
    var b: Vec2 = Vec2(3, 4)
    var c: Vec2 = a + b
    print(String(c))
    print(c == Vec2(4, 6))
    print(c == Vec2(0, 0))
