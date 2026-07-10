# Keyword-only parameters after '*' must be supplied by name.
# expect: expects 1 argument(s), got 2
def kw_only(a: Int, *, b: Int) -> Int:
    return a + b

def main():
    print(kw_only(1, 2))
