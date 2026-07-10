# Same-arity type-directed overloads work when exactly one overload is best, but
# a literal that can coerce to both candidates is ambiguous.

def show(x: Int) -> Int:
    return x

def show(x: Float64) -> Float64:
    return x

def main():
    print(show(1))
