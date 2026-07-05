# Keyword-only parameters via the '*' marker (Mojo functions manual).
# expect: keyword-only
def kw_only_args(a1: Int, a2: Int, *, double: Bool) -> Int:
    return a1 * a2
