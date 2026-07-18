# An otherwise-unconstrained integer literal materializes as its contextual
# default type Int, so it selects the Int overload over Float64 instead of
# reporting an ambiguity.

def show(x: Int) -> Int:
    return x

def show(x: Float64) -> Float64:
    return x

def main():
    print(show(1))
