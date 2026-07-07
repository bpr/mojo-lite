@fieldwise_init
struct Thing:
    var x: Int

def main():
    var a: Thing = Thing(1)
    var b: Thing = a^
    print(b.x)
