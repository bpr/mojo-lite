# expect: use of 'p' after it was transferred
@fieldwise_init
struct T:
    var x: Int

def main():
    var p: T = T(1)
    var q: T = p^
    p.x = 5
