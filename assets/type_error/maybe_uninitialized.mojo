# Definite initialization requires every reaching branch to initialize a value.
# expect: may be uninitialized
def choose(flag: Bool):
    var value: Int
    if flag:
        value = 42
    print(value)
