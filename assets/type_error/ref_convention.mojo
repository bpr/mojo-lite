# The `ref` argument convention (parametric-mutability reference) — parsed,
# semantics deferred. An origin specifier `ref[origin] x` is parsed and discarded.
# expect: argument conventions
def peek(ref[_] x: Int) -> Int:
    return x
