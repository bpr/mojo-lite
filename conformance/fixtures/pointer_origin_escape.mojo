# Both implementations reject returning a pointer whose inferred origin is a
# local: the pinned nightly removed implicit widening to unsafe-any origins,
# and Mojito reports the escape directly.
def escape() -> UnsafePointer[Int]:
    var local = 7
    return UnsafePointer(to=local)

def main():
    print(1)
