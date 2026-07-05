# `with` statement (context manager) — parsed and grammar-documented, semantics deferred.
# expect: with statement
def read_all(path: String) -> String:
    with open(path, "r") as f:
        return f.read()
