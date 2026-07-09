# VM-backed CTFE handles straight-line/branching expressions that the old AST
# CTFE island deliberately does not model, such as ternary expressions.
def choose(n: Int) -> Int:
    return 11 if n > 0 else 22

comptime X = choose(5)

def main():
    print(X)
