# Positional-only parameters via the '/' marker (Mojo functions manual).
def first(a: Int, b: Int, /) -> Int:
    return a

def mix(a: Int, /, b: Int, *, scale: Int) -> Int:
    return (a + b) * scale

def main():
    print(first(7, 9))
    print(mix(2, b=3, scale=4))
