# A pointer to an immutable parameter has immutable provenance, so stores
# through it are rejected.
# expect: immutable origin
def observe(x: Int):
    var p = UnsafePointer(to=x)
    p[0] = 1

def main():
    observe(3)
