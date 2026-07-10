# expect: operator '<'
# Phase 4: a plain `T: Equatable` grants equality but not ordering — comparing
# with `<` is a type error (only `Comparable` promises an ordering).
def less[T: Equatable](a: T, b: T) -> Bool:
    return a < b

def main():
    print(less(1, 2))
