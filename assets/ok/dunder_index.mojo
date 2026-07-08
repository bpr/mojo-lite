# `len(p)` and `p[i]` on a user struct dispatch to `__len__` / `__getitem__`.
@fieldwise_init
struct Pair:
    var a: Int
    var b: Int

    def __len__(self) -> Int:
        return 2

    def __getitem__(self, i: Int) -> Int:
        if i == 0:
            return self.a
        return self.b

def main():
    var p: Pair = Pair(10, 20)
    print(len(p))
    print(p[0])
    print(p[1])
