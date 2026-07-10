# expect: does not conform to trait 'Copyable'
@fieldwise_init
struct Handle:
    var id: Int

def duplicate[T: Copyable](x: T) -> T:
    var y: T = x
    return y

def main():
    var h: Handle = Handle(1)
    var h2: Handle = duplicate(h)
    print(h2.id)
