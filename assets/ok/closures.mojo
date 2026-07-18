# Nested `def`s (closures) are lifted to functions whose captured enclosing locals
# carry explicit immutable or mutable environments, so reads, writes,
# self-recursion, and calls to top-level functions execute through the VM.
def double(x: Int) -> Int:
    return x * 2

def adder(n: Int) -> Int:
    def add_n(x: Int) unified {imm n} -> Int:
        return double(x) + n
    return add_n(100)

def counter() -> Int:
    var total: Int = 0
    def add(x: Int) unified {mut total}:
        total = total + x
    add(5)
    add(3)
    return total

def factorial(base: Int) -> Int:
    def fact(n: Int) unified {imm base} -> Int:
        if n <= 1:
            return base
        return n * fact(n - 1)
    return fact(5)

def main():
    print(adder(21))
    print(counter())
    print(factorial(1))
