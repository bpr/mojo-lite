# Re-initializing a moved-out field makes the whole value usable again.
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
    p.a = Inner(9)
    var q: Pair = p^
    print(q.a.id)
    print(q.b.id)
