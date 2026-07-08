# A user type is iterable via the Mojo iterator protocol: `for x in c` calls
# `c.__iter__()` to get an iterator, then loops while `len(iter) > 0`, binding
# `x = iter.__next__()` (which advances the iterator in place, `mut self`).
@fieldwise_init
struct RangeIter:
    var cur: Int
    var stop: Int

    def __len__(self) -> Int:
        return self.stop - self.cur

    def __next__(mut self) -> Int:
        var v: Int = self.cur
        self.cur = self.cur + 1
        return v

@fieldwise_init
struct Countdown:
    var n: Int

    def __iter__(self) -> RangeIter:
        return RangeIter(0, self.n)

def main():
    var total: Int = 0
    for x in Countdown(5):
        if x == 4:
            continue
        total = total + x
    print(total)
