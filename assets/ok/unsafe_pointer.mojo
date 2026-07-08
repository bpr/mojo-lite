# `UnsafePointer[T]` — low-level heap storage. `alloc(n)` reserves n slots;
# `ptr[i]` loads/stores; a copied pointer *aliases* the same storage (unlike a
# value type); `free()` releases it.
def main():
    var p: UnsafePointer[Int] = UnsafePointer[Int].alloc(4)
    p[0] = 10
    p[1] = 20
    p[2] = 30
    p[2] += 5
    print(p[0], p[1], p[2])
    var q: UnsafePointer[Int] = p
    q[0] = 99
    print(p[0])
    p.free()
