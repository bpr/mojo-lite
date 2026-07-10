# expect: does not conform to trait 'ImplicitlyCopyable'
struct ExplicitBox(Copyable):
    var n: Int

    def __init__(out self, n: Int):
        self.n = n

    def __init__(out self, *, copy: Self):
        self.n = copy.n

def duplicate_implicit[T: ImplicitlyCopyable](x: T) -> T:
    var y: T = x
    return y

def main():
    var b: ExplicitBox = ExplicitBox(3)
    var b2: ExplicitBox = duplicate_implicit(b)
    print(b2.n)
