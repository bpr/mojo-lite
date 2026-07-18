# expect: use after
def main():
    var pointer = UnsafePointer[Int].alloc(1)
    var alias = pointer
    pointer.free()
    print(alias[0])
