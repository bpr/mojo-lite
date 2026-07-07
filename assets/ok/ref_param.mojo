# The `ref` argument convention (a reference); an origin specifier `ref[origin] x`
# is parsed and discarded.
def peek(ref[_] x: Int) -> Int:
    return x
