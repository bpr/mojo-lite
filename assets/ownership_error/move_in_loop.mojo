# expect: transferred
@fieldwise_init
struct Thing:
    var x: Int

def main():
    var a: Thing = Thing(1)
    for i in range(3):
        var b: Thing = a^
        print(b.x)
