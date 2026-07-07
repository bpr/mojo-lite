# Nested `def`s (closures) are lifted to functions whose captured enclosing locals
# become leading `mut` parameters, so read-capture, write-capture (reference
# semantics), self-recursion, and calling top-level functions all run identically
# on the tree-walker and the VM.
def double(x: Int) -> Int:
    return x * 2

def adder(n: Int) -> Int:
    def add_n(x: Int) -> Int:
        return double(x) + n
    return add_n(100)

def counter() -> Int:
    var total: Int = 0
    def add(x: Int):
        total = total + x
    add(5)
    add(3)
    return total

def factorial(base: Int) -> Int:
    def fact(n: Int) -> Int:
        if n <= 1:
            return base
        return n * fact(n - 1)
    return fact(5)

def main():
    print(adder(21))
    print(counter())
    print(factorial(1))
