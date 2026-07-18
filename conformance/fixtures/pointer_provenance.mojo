def main():
    var base = UnsafePointer[Int].alloc_aligned(4, 16)
    base[0] = 10
    base[1] = 20
    var next = base + 1
    print(next[0], next - base, next == base + 1)
    base.free()
