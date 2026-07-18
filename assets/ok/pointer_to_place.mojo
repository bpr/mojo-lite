# UnsafePointer(to=place) infers the place's origin, loans the owner, and
# aliases its storage: stores through the pointer are visible in the source.
def main():
    var x = 42
    var p = UnsafePointer(to=x)
    print(p[0])
    p[0] = 7
    p[0] += 1
    print(x)
