# A field moved out (`p.a^`) leaves the sibling `p.b` usable — field-sensitive
# partial-move analysis.
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
    print(x.id)
    print(p.b.id)
