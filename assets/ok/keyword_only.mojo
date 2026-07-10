# Keyword-only parameters via the '*' marker (Mojo functions manual).
def kw_only_args(a1: Int, a2: Int, *, double: Bool) -> Int:
    if double:
        return a1 * a2 * 2
    return a1 * a2

def with_default(a: Int = 1, *, scale: Int) -> Int:
    return a * scale

def with_variadic(*values: Int, scale: Int) -> Int:
    var total: Int = 0
    for value in values:
        total = total + value
    return total * scale

def main():
    print(kw_only_args(3, 4, double=True))
    print(with_default(scale=5))
    print(with_variadic(1, 2, 3, scale=10))
