# Arity- and type-distinct overloads: top-level functions, constructors, and
# methods.

def choose() -> Int:
    return 1

def choose(x: Int) -> Int:
    return x + 1

def choose(x: String) -> String:
    return x + "!"

struct Box:
    var n: Int

    def __init__(out self):
        self.n = 0

    def __init__(out self, n: Int):
        self.n = n

    def __init__(out self, label: String):
        self.n = len(label)

    def value(self) -> Int:
        return self.n

    def value(self, add: Int) -> Int:
        return self.n + add

    def value(self, suffix: String) -> String:
        return String(self.n) + suffix

def main():
    var s: String = "go"
    print(choose(), choose(41), choose(s))
    var a: Box = Box()
    var b: Box = Box(5)
    var c: Box = Box(s)
    print(a.value(), b.value(7), c.value(" lanes"))
