# Duplicate-equivalent overloads: parameter types are the signature, so a
# return-type-only difference is a redeclaration.
# expect: already declared
def f(x: Int) -> Int:
    return x

def f(x: Int) -> Float64:
    return 1.5
