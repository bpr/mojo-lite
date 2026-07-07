# The utility numeric built-ins run identically on the tree-walker and the VM.
def main():
    print(abs(-5))
    print(abs(-3.5))
    print(min(3, 7), max(3, 7))
    print(min(2.5, 1.5))
    print(round(2.5), round(2.4))
    print(Int(3.9), UInt(42), Float64(7))
    var got: Int = abs(min(-4, -2))
    print(got)
