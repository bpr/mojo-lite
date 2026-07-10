# expect: operator '=='
# Phase 6 design decision: `Hashable` does not imply `Equatable`. A hash-backed
# collection can bucket by hash, but resolving a collision needs equality — so a
# key type must bound `Hashable & Equatable`. `Hashable` alone cannot use `==`.
def same[K: Hashable](a: K, b: K) -> Bool:
    return a == b

def main():
    print(same(1, 1))
