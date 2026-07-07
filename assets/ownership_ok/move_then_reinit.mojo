@fieldwise_init
struct Thing:
    var x: Int

def main():
    var a: Thing = Thing(1)
    var b: Thing = a^
    a = Thing(2)
    print(a.x)
