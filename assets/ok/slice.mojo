# Slice subscripts `a[i:j:k]` on List/String (Python semantics: negative indices,
# optional bounds, negative step reverses).
def mid(xs: List[Int]) -> List[Int]:
    return xs[1:3]

def main():
    var xs: List[Int] = [0, 1, 2, 3, 4]
    print(mid(xs))
    print(xs[::-1])
    print(xs[-2:])
    var s: String = "hello"
    print(s[1:4])
    print(s[::-1])
