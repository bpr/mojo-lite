# All aliases share allocation provenance and observe deallocation.
# expect: double free
def main():
    var pointer = UnsafePointer[Int].alloc(1)
    var alias = pointer
    pointer.free()
    alias.free()
