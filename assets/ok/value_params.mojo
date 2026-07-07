# Value-parameterized generics run identically on the tree-walker and the VM: a
# struct reifies its value parameter onto the instance (read back via `Self.size`),
# and a value-parameterized function binds it as a frame-local `Int`.
@fieldwise_init
struct FixedBuffer[size: Int]:
    var tag: Int
    def capacity(self) -> Int:
        return Self.size

def scaled[factor: Int](x: Int) -> Int:
    return x * factor

def main():
    var b: FixedBuffer[8] = FixedBuffer[8](3)
    print(b.capacity(), b.tag)
    var c: FixedBuffer[2 + 3] = FixedBuffer[2 + 3](0)
    print(c.capacity())
    print(scaled[10](4))
