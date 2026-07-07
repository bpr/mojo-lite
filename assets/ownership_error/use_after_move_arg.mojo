# expect: use of 'a' after it was transferred
@fieldwise_init
struct Thing:
    var x: Int

def consume(t: Thing) -> Int:
    return t.x

def main():
    var a: Thing = Thing(7)
    var got: Int = consume(a^)
    print(a.x)
