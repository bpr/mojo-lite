# `x in c` / `x not in c` on a user struct dispatch to `__contains__`.
@fieldwise_init
struct Pair:
    var a: Int
    var b: Int

    def __contains__(self, x: Int) -> Bool:
        return self.a == x or self.b == x

def main():
    var p: Pair = Pair(3, 7)
    print(3 in p)
    print(4 not in p)
    print(7 in p)
