# expect: use of 'p.a'
@fieldwise_init
struct Inner:
    var id: Int

@fieldwise_init
struct Pair:
    var a: Inner
    var b: Inner

def main():
    var p: Pair = Pair(Inner(1), Inner(2))
    var x: Inner = p.a^
    print(p.a.id)
