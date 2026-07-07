# SIMD lane writes through places — a bare vector lane and a lane reached through
# a struct field — run identically on the tree-walker and the VM.
@fieldwise_init
struct Vec4:
    var data: SIMD[DType.int32, 4]

def main():
    var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)
    v[0] = 10
    v[2] += 5
    print(v[0], v[1], v[2], v[3])
    var w: Vec4 = Vec4(SIMD[DType.int32, 4](0, 0, 0, 0))
    w.data[1] = 42
    w.data[1] += 8
    print(w.data[0], w.data[1])
