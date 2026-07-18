# Context-manager elaboration still type-checks the manager expression.
# expect: Undefined variable 'open'
def read_all(path: String) -> String:
    with open(path, "r") as f:
        return f.read()
