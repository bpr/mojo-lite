# Positional-only parameters before '/' cannot be supplied by keyword.
# expect: positional-only
def first(a: Int, b: Int, /) -> Int:
    return a

def main():
    print(first(a=1, b=2))
