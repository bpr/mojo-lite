# expect: use of 'a' after it was transferred
@fieldwise_init
struct Thing:
    var x: Int

def main():
    var a: Thing = Thing(1)
    var b: Thing = a^
    print(a.x)
