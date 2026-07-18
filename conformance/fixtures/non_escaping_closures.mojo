@fieldwise_init
struct Scale(def(Int) -> Int):
    var factor: Int
    def __call__(self, value: Int) -> Int:
        return value * self.factor

def apply[T: Copyable & Movable](callback: def(T) -> T, value: T) -> T:
    return callback(value)

def nested() -> Int:
    var base = 10
    def helper[T: Copyable & Movable](value: T) unified {base} -> T:
        return value
    def caller(value: Int) unified {helper, base} -> Int:
        return helper(value) + base
    return caller(32)

def main():
    print(nested())
    print(apply(Scale(3), 14))
