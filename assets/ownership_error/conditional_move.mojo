# expect: may have been transferred
@fieldwise_init
struct Thing:
    var x: Int

def main():
    var flag: Bool = True
    var a: Thing = Thing(1)
    if flag:
        var b: Thing = a^
    print(a.x)
