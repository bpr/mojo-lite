# Same-arity type-directed overloads work when exactly one overload is best.
# An integer literal defaults to Int, but between UInt and Float64 neither
# candidate is the literal's contextual default type, so the call is ambiguous.
# expect: ambiguous overloaded call

def show(x: UInt) -> Int:
    return 0

def show(x: Float64) -> Int:
    return 1

def main():
    print(show(1))
