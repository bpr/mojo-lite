@fieldwise_init
struct Matrix:
    var value: Int
    def __matmul__(self, other: Matrix) -> Matrix:
        return Matrix(self.value * other.value)

def main():
    print(6 & 3, 6 | 3, 6 ^ 3, 1 << 4, 16 >> 2)
    print((Matrix(6) @ Matrix(7)).value)
