# A `mut` reference parameter mutates the caller's variable (write-back).
def incr(mut x: Int, by: Int):
    x = x + by

@fieldwise_init
struct Counter:
    var n: Int

def bump(mut c: Counter, k: Int):
    c.n = c.n + k

def main():
    var n: Int = 10
    incr(n, 5)
    incr(n, 3)
    print(n)
    var c: Counter = Counter(0)
    bump(c, 7)
    print(c.n)
