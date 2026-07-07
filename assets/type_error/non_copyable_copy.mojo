# expect: cannot copy non-Copyable
@fieldwise_init
struct Handle:
    var id: Int

def main():
    var a: Handle = Handle(1)
    var b: Handle = a
    print(b.id)
