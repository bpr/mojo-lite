# VM-backed CTFE follows a transitive pure helper call graph under fuel.
# `dec` uses a ternary expression, which the old AST CTFE interpreter does not
# support; `sumdown` calls it recursively.
def dec(n: Int) -> Int:
    return n - 1 if n > 0 else 0

def sumdown(n: Int) -> Int:
    if n == 0:
        return 0
    return n + sumdown(dec(n))

comptime TOTAL = sumdown(4)

def main():
    print(TOTAL)
