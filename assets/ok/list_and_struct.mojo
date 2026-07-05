@fieldwise_init
struct Counter:
    var n: Int

    def bump(mut self, k: Int):
        self.n += k

var c: Counter = Counter(0)
c.bump(5)
c.bump(37)

var xs: List[Int] = [1, 2, 3]
xs.append(4)
var total: Int = 0
for x in xs:
    total += x
print("counter:", c.n, "total:", total)
