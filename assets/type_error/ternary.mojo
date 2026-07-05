# Conditional expression (ternary) — parsed, semantics deferred.
# expect: conditional expression
def clamp(x: Int, lo: Int, hi: Int) -> Int:
    return lo if x < lo else hi if x > hi else x
