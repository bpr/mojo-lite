def transform(value: Int) -> Int:
    return value + 1

def transform(value: String) raises -> Int:
    raise Error("string overload")

def identity[T: Copyable & Movable](value: T) -> T:
    return value

def main():
    var integer_transform: def(Int) -> Int = transform
    var generic_integer: def(Int) -> Int = identity
    print(integer_transform(generic_integer(41)))
    var raising_transform: def(String) raises -> Int = transform
    try:
        print(raising_transform("selected by type and effect"))
    except:
        print("caught")
