# Conditional expression (ternary): `a if cond else b`. Nests right, and the two
# branches unify to a common type.
def clamp(x: Int, lo: Int, hi: Int) -> Int:
    return lo if x < lo else hi if x > hi else x

def main():
    print(clamp(5, 0, 10))
    print(clamp(-3, 0, 10))
    print(clamp(99, 0, 10))
