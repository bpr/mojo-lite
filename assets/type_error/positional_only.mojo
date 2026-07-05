# Positional-only parameters via the '/' marker (Mojo functions manual).
# expect: positional-only
def first(a: Int, b: Int, /) -> Int:
    return a
